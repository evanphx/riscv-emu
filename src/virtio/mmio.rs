//! VirtIO MMIO transport (version 2).

use crate::dev::Plic;
use crate::isa::Exception;
use crate::logger::{Level, Log};
use crate::memory::Memory;
use crate::mmu::{Mmio, Shm};
use std::collections::BTreeMap;
use std::fmt;
use std::sync::{Arc, Mutex, Weak};

pub const MAGIC: u32 = 0x74726976; // "virt"
pub const VERSION: u32 = 0x2;
pub const VENDOR_ID: u32 = 0x476f4d55; // "GoMU"

pub const VIRTIO_F_INDIRECT_DESC: u32 = 28;
pub const VIRTIO_F_VERSION_1: u32 = 32;

/// Maximum queue size.
pub const QUEUE_NUM_MAX: u32 = 512;

// Device status bits.
pub const ACKNOWLEDGE: u32 = 1;
pub const DRIVER: u32 = 2;
pub const DRIVER_OK: u32 = 4;
pub const FEATURES_OK: u32 = 8;
pub const DEVICE_NEEDS_RESET: u32 = 64;
pub const FAILED: u32 = 128;

/// Descriptor flags.
pub const VIRTQ_DESC_F_NEXT: u16 = 1;
pub const VIRTQ_DESC_F_WRITE: u16 = 2;
pub const VIRTQ_DESC_F_INDIRECT: u16 = 4;

/// Returns a description of the device status bits.
pub fn status_string(status: u32) -> String {
    let names = [
        (ACKNOWLEDGE, "ACKNOWLEDGE"),
        (DRIVER, "DRIVER"),
        (FAILED, "FAILED"),
        (FEATURES_OK, "FEATURES_OK"),
        (DRIVER_OK, "DRIVER_OK"),
        (DEVICE_NEEDS_RESET, "DEVICE_NEEDS_RESET"),
    ];
    names
        .iter()
        .filter(|(b, _)| status & b != 0)
        .map(|(_, n)| *n)
        .collect::<Vec<_>>()
        .join(",")
}

/// File size formatted with decimal units.
pub struct FileSize(pub u64);

impl fmt::Display for FileSize {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let s = self.0;
        if s > 1000 * 1000 * 1000 * 1000 {
            write!(f, "{} TB", s / (1000 * 1000 * 1000 * 1000))
        } else if s > 1000 * 1000 * 1000 {
            write!(f, "{} GB", s / (1000 * 1000 * 1000))
        } else if s > 1000 * 1000 {
            write!(f, "{} MB", s / (1000 * 1000))
        } else if s > 1000 {
            write!(f, "{} kB", s / 1000)
        } else {
            write!(f, "{} B", s)
        }
    }
}

/// Virtqueue descriptor.
#[derive(Clone, Copy, Debug, Default)]
pub struct Desc {
    pub addr: u64,
    pub len: u32,
    pub flags: u16,
    pub next: u16,
}

impl fmt::Display for Desc {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut flags = Vec::new();
        if self.flags & VIRTQ_DESC_F_INDIRECT != 0 {
            flags.push("INDIRECT");
        }
        if self.flags & VIRTQ_DESC_F_WRITE != 0 {
            flags.push("WRITE");
        }
        if self.flags & VIRTQ_DESC_F_NEXT != 0 {
            flags.push("NEXT");
        }
        write!(
            f,
            "Buf={}@{:x},Flags={}[{:x}],Next={:x}",
            self.len,
            self.addr,
            flags.join(","),
            self.flags,
            self.next
        )
    }
}

/// A virtqueue.
#[derive(Clone, Copy, Debug, Default)]
pub struct Queue {
    pub index: u32,
    /// Queue size set by the guest.
    pub num: u32,
    pub ready: u32,
    /// Guest physical address of the descriptor table.
    pub desc_phys: u64,
    /// Guest physical address of the available ring.
    pub avail_phys: u64,
    /// Guest physical address of the used ring.
    pub used_phys: u64,
    pub last_avail_idx: u16,
}

/// Guest memory accessor used by the device implementations.
#[derive(Clone)]
pub struct GuestMem {
    pub mem: Arc<Memory>,
}

impl GuestMem {
    pub fn read_u8(&self, addr: u64) -> Result<u8, String> {
        self.mem
            .load_u8(addr)
            .ok_or_else(|| format!("invalid addr {:x}", addr))
    }

    pub fn read_u16(&self, addr: u64) -> Result<u16, String> {
        self.mem
            .load_u16(addr)
            .ok_or_else(|| format!("invalid addr {:x}", addr))
    }

    pub fn read_u32(&self, addr: u64) -> Result<u32, String> {
        self.mem
            .load_u32(addr)
            .ok_or_else(|| format!("invalid addr {:x}", addr))
    }

    pub fn read_u64(&self, addr: u64) -> Result<u64, String> {
        self.mem
            .load_u64(addr)
            .ok_or_else(|| format!("invalid addr {:x}", addr))
    }

    pub fn write_u8(&self, addr: u64, v: u8) -> Result<(), String> {
        self.mem
            .store_u8(addr, v)
            .ok_or_else(|| format!("invalid addr {:x}", addr))
    }

    pub fn write_u16(&self, addr: u64, v: u16) -> Result<(), String> {
        self.mem
            .store_u16(addr, v)
            .ok_or_else(|| format!("invalid addr {:x}", addr))
    }

    pub fn write_u32(&self, addr: u64, v: u32) -> Result<(), String> {
        self.mem
            .store_u32(addr, v)
            .ok_or_else(|| format!("invalid addr {:x}", addr))
    }

    /// Copies guest memory into buf.
    pub fn read(&self, addr: u64, buf: &mut [u8]) -> Result<(), String> {
        if !self.mem.contains_range(addr, buf.len() as u64) {
            return Err(format!("invalid addr {:x}+{}", addr, buf.len()));
        }
        self.mem.read_bytes(self.mem.offset(addr), buf);
        Ok(())
    }

    /// Copies data into guest memory.
    pub fn write(&self, addr: u64, data: &[u8]) -> Result<(), String> {
        if !self.mem.contains_range(addr, data.len() as u64) {
            return Err(format!("invalid addr {:x}+{}", addr, data.len()));
        }
        self.mem.write_bytes(self.mem.offset(addr), data);
        Ok(())
    }

    /// Returns the guest buffer as a mutable slice for device DMA.
    ///
    /// # Safety
    ///
    /// The buffer must belong to a descriptor owned by the device.
    pub unsafe fn slice_mut(&self, addr: u64, len: u64) -> Result<&mut [u8], String> {
        self.mem
            .guest_slice_mut(addr, len)
            .ok_or_else(|| format!("invalid addr {:x}+{}", addr, len))
    }

    /// Returns the guest buffer as a slice for device DMA.
    ///
    /// # Safety
    ///
    /// The buffer must belong to a descriptor owned by the device.
    pub unsafe fn slice(&self, addr: u64, len: u64) -> Result<&[u8], String> {
        self.mem
            .guest_slice(addr, len)
            .ok_or_else(|| format!("invalid addr {:x}+{}", addr, len))
    }

    /// Loads the descriptor idx of the queue.
    pub fn load_desc(&self, vq: &Queue, idx: u16) -> Result<Desc, String> {
        let d = vq.desc_phys + idx as u64 * 16;
        Ok(Desc {
            addr: self.read_u64(d)?,
            len: self.read_u32(d + 8)?,
            flags: self.read_u16(d + 12)?,
            next: self.read_u16(d + 14)?,
        })
    }
}

/// The transport state shared by all VirtIO devices.
pub struct VirtioState {
    pub log: Log,
    pub device_id: u32,
    pub features: u32,
    pub start: u64,
    pub end: u64,
    pub plic: Plic,
    pub irq: u32,
    pub guest: GuestMem,
    pub shm: BTreeMap<u32, Arc<Mutex<Vec<u8>>>>,
    pub shm_ranges: BTreeMap<u32, (u64, u64)>,

    shm_sel: u32,
    device_features_sel: u32,
    driver_features_sel: u32,
    pub driver_features: [u32; 2],
    pub status: u32,
    pub interrupt_status: u32,
    queue_sel: u32,
    pub queues: Vec<Queue>,
    /// Incremented on device reset. Asynchronous workers use it to
    /// discard completions of requests started before the reset.
    pub generation: u64,
}

impl VirtioState {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        name: &str,
        device_id: u32,
        features: u32,
        start: u64,
        size: u64,
        plic: Plic,
        irq: u32,
        mem: Arc<Memory>,
        num_queues: usize,
    ) -> VirtioState {
        VirtioState {
            log: Log::new(name, Level::Error),
            device_id,
            features,
            start,
            end: start + size,
            plic,
            irq,
            guest: GuestMem { mem },
            shm: BTreeMap::new(),
            shm_ranges: BTreeMap::new(),
            shm_sel: 0,
            device_features_sel: 0,
            driver_features_sel: 0,
            driver_features: [0; 2],
            status: 0,
            interrupt_status: 0,
            queue_sel: 0,
            queues: (0..num_queues)
                .map(|i| Queue {
                    index: i as u32,
                    ..Default::default()
                })
                .collect(),
            generation: 0,
        }
    }

    pub fn driver_ok(&self) -> bool {
        self.status & DRIVER_OK != 0
    }

    /// Tests if the driver has acknowledged the feature bit.
    pub fn driver_feature(&self, bit: u32) -> bool {
        self.driver_features[(bit / 32) as usize & 1] & (1 << (bit % 32)) != 0
    }

    /// Raises the used buffer notification interrupt unless the
    /// driver has suppressed interrupts on the queue.
    fn notify(&mut self, q: usize) -> Result<(), String> {
        let avail_flags = self.guest.read_u16(self.queues[q].avail_phys)?;
        // Bit 0 of avail flags is VIRTQ_AVAIL_F_NO_INTERRUPT.
        if avail_flags & 1 == 0 {
            self.interrupt_status |= 0x1;
            self.plic.set_interrupt_request(self.irq, true);
        }
        Ok(())
    }

    /// Raises the configuration change interrupt.
    pub fn notify_config(&mut self) {
        self.interrupt_status |= 0x2;
        self.plic.set_interrupt_request(self.irq, true);
    }

    /// Pops the next available descriptor chain head of the queue.
    pub fn pop_avail(&mut self, q: usize) -> Result<Option<u16>, String> {
        let vq = self.queues[q];
        if vq.num == 0 || vq.ready == 0 {
            return Ok(None);
        }
        let avail_idx = self.guest.read_u16(vq.avail_phys + 2)?;
        if vq.last_avail_idx == avail_idx {
            return Ok(None);
        }
        let ofs = (vq.last_avail_idx as u32 % vq.num) as u64;
        let head = self.guest.read_u16(vq.avail_phys + 4 + ofs * 2)?;
        self.queues[q].last_avail_idx = vq.last_avail_idx.wrapping_add(1);
        Ok(Some(head))
    }

    /// Adds the descriptor chain to the used ring and notifies the
    /// guest.
    pub fn complete_descriptor(&mut self, q: usize, desc: u16, tx: u32) {
        if let Err(e) = self.push_used(q, desc, tx).and_then(|_| self.notify(q)) {
            self.log.error(format_args!("complete descriptor: {}", e));
        }
    }

    fn push_used(&mut self, q: usize, desc: u16, tx: u32) -> Result<(), String> {
        let vq = self.queues[q];
        let used_idx_addr = vq.used_phys + 2;
        let used_idx = self.guest.read_u16(used_idx_addr)?;
        let elem = vq.used_phys + 4 + (used_idx as u32 % vq.num) as u64 * 8;
        self.guest.write_u32(elem, desc as u32)?;
        self.guest.write_u32(elem + 4, tx)?;
        self.guest.write_u16(used_idx_addr, used_idx.wrapping_add(1))
    }

    /// Selected shared memory region.
    fn shm_range(&self) -> Option<(u64, u64)> {
        self.shm_ranges.get(&self.shm_sel).copied()
    }

    fn load32(&mut self, offset: u64) -> u32 {
        match offset {
            0x000 => MAGIC,
            0x004 => VERSION,
            0x008 => self.device_id,
            0x00c => VENDOR_ID,
            0x010 => match self.device_features_sel {
                0 => self.features,
                // Bit 32 is VIRTIO_F_VERSION_1.
                1 => 1,
                _ => 0,
            },
            0x034 => {
                if (self.queue_sel as usize) < self.queues.len() {
                    QUEUE_NUM_MAX
                } else {
                    0
                }
            }
            0x044 => self
                .queues
                .get(self.queue_sel as usize)
                .map(|q| q.ready)
                .unwrap_or(0),
            0x060 => self.interrupt_status,
            0x070 => self.status,
            0x0b0 => match self.shm_range() {
                Some((s, e)) => (e - s) as u32,
                None => 0xffff_ffff,
            },
            0x0b4 => match self.shm_range() {
                Some((s, e)) => ((e - s) >> 32) as u32,
                None => 0xffff_ffff,
            },
            0x0b8 => match self.shm_range() {
                Some((s, _)) => s as u32,
                None => 0xffff_ffff,
            },
            0x0bc => match self.shm_range() {
                Some((s, _)) => (s >> 32) as u32,
                None => 0xffff_ffff,
            },
            _ => 0,
        }
    }
}

/// VirtIO device implementation.
pub trait Handler: Send {
    /// Called once after the device has been created.
    fn attach(&mut self, _dev: Weak<Mutex<VirtioDevice>>) {}

    /// The driver set DRIVER_OK.
    fn ready(&mut self, _st: &mut VirtioState) {}

    /// The driver reset the device.
    fn reset(&mut self, _st: &mut VirtioState) -> Result<(), String> {
        Ok(())
    }

    /// Prints device statistics.
    fn device_stats(&self, _st: &VirtioState) {}

    /// Executes the descriptor chain starting at idx. Returns the
    /// number of bytes written to the guest buffers, or 0 if the
    /// chain is completed asynchronously.
    fn execute_descriptor_chain(
        &mut self,
        st: &mut VirtioState,
        q: usize,
        idx: u16,
    ) -> Result<u32, String>;

    /// Reads the device configuration space (offset >= 0x100).
    fn config_load(&mut self, _st: &mut VirtioState, _offset: u64, _size: u64) -> Option<u64> {
        None
    }

    /// Writes the device configuration space (offset >= 0x100).
    fn config_store(&mut self, _st: &mut VirtioState, _offset: u64, _size: u64, _v: u64) -> bool {
        false
    }
}

/// A VirtIO device: the transport state and the device handler.
pub struct VirtioDevice {
    pub state: VirtioState,
    pub handler: Box<dyn Handler>,
}

impl VirtioDevice {
    /// Creates a new device and attaches the handler to it.
    pub fn new(state: VirtioState, handler: Box<dyn Handler>) -> Arc<Mutex<VirtioDevice>> {
        let dev = Arc::new(Mutex::new(VirtioDevice { state, handler }));
        let weak = Arc::downgrade(&dev);
        dev.lock().unwrap().handler.attach(weak);
        dev
    }

    /// Processes the available descriptors of the queue.
    pub fn process_queue(&mut self, idx: u32) {
        let st = &mut self.state;
        crate::log_debug!(st.log, "ProcessQueue({})", idx);
        let q = idx as usize;
        if q >= st.queues.len() {
            st.log
                .error(format_args!("invalid queue index {}", idx));
            return;
        }
        let mut processed = false;
        loop {
            let head = match st.pop_avail(q) {
                Ok(Some(h)) => h,
                Ok(None) => break,
                Err(e) => {
                    st.log.error(format_args!("guest memory access: {}", e));
                    return;
                }
            };
            match self.handler.execute_descriptor_chain(st, q, head) {
                Ok(0) => {
                    // Asynchronous processing.
                    crate::log_debug!(st.log, "chain idx={} asynchronous processing", head);
                }
                Ok(tx) => {
                    if let Err(e) = st.push_used(q, head, tx) {
                        st.log.error(format_args!("guest memory access: {}", e));
                        return;
                    }
                    processed = true;
                }
                Err(e) => {
                    st.log
                        .error(format_args!("execute descriptor chain: {}", e));
                    return;
                }
            }
        }
        if processed {
            if let Err(e) = st.notify(q) {
                st.log.error(format_args!("guest memory access: {}", e));
            }
        }
    }

    fn load(&mut self, offset: u64, size: u64) -> Result<u64, Exception> {
        if offset >= 0x100 {
            if let Some(v) = self.handler.config_load(&mut self.state, offset, size) {
                return Ok(v);
            }
        }
        if size != 4 {
            return Err(Exception::fatal(format!(
                "MMIO.Load{}({:x})",
                size * 8,
                offset
            )));
        }
        crate::log_debug!(self.state.log, "Load32({})", mmio_reg(offset));
        Ok(self.state.load32(offset) as u64)
    }

    fn store(&mut self, offset: u64, size: u64, v: u64) -> Result<(), Exception> {
        if offset >= 0x100 && self.handler.config_store(&mut self.state, offset, size, v) {
            return Ok(());
        }
        if size != 4 {
            return Err(Exception::fatal(format!(
                "MMIO.Store{}({:x}, 0x{:x})",
                size * 8,
                offset,
                v
            )));
        }
        let v = v as u32;
        crate::log_debug!(self.state.log, "Store32({}, 0x{:08x})", mmio_reg(offset), v);
        let st = &mut self.state;
        let sel = st.queue_sel as usize;
        match offset {
            0x014 => st.device_features_sel = v,
            0x024 => st.driver_features_sel = v,
            0x020 => {
                if st.driver_features_sel < 2 {
                    st.driver_features[st.driver_features_sel as usize] = v;
                }
            }
            0x030 => st.queue_sel = v,
            0x038 => {
                if let Some(q) = st.queues.get_mut(sel) {
                    q.num = v;
                }
            }
            0x044 => {
                if let Some(q) = st.queues.get_mut(sel) {
                    q.ready = v;
                }
            }
            0x050 => self.process_queue(v),
            0x064 => {
                // InterruptACK: the guest acknowledges the interrupts.
                st.interrupt_status &= !v;
                let pending = st.interrupt_status != 0;
                st.plic.set_interrupt_request(st.irq, pending);
            }
            0x070 => {
                if v == 0 {
                    // Device reset.
                    self.handler
                        .reset(&mut self.state)
                        .map_err(Exception::fatal)?;
                    let st = &mut self.state;
                    st.plic.reset(st.irq);
                    st.device_features_sel = 0;
                    st.driver_features_sel = 0;
                    st.driver_features = [0; 2];
                    st.status = 0;
                    st.interrupt_status = 0;
                    st.queue_sel = 0;
                    st.generation += 1;
                    for q in st.queues.iter_mut() {
                        q.num = 0;
                        q.ready = 0;
                        q.desc_phys = 0;
                        q.avail_phys = 0;
                        q.used_phys = 0;
                        q.last_avail_idx = 0;
                    }
                    return Ok(());
                }
                st.status |= v;
                if v & FEATURES_OK != 0 && st.driver_features[1] & 1 == 0 {
                    // The driver did not accept VIRTIO_F_VERSION_1.
                    st.status &= !FEATURES_OK;
                }
                if v & DRIVER_OK != 0 {
                    self.handler.ready(&mut self.state);
                }
            }
            0x080 => {
                if let Some(q) = st.queues.get_mut(sel) {
                    q.desc_phys = (q.desc_phys & 0xffff_ffff_0000_0000) | v as u64;
                }
            }
            0x084 => {
                if let Some(q) = st.queues.get_mut(sel) {
                    q.desc_phys = (q.desc_phys & 0xffff_ffff) | (v as u64) << 32;
                }
            }
            0x090 => {
                if let Some(q) = st.queues.get_mut(sel) {
                    q.avail_phys = (q.avail_phys & 0xffff_ffff_0000_0000) | v as u64;
                }
            }
            0x094 => {
                if let Some(q) = st.queues.get_mut(sel) {
                    q.avail_phys = (q.avail_phys & 0xffff_ffff) | (v as u64) << 32;
                }
            }
            0x0a0 => {
                if let Some(q) = st.queues.get_mut(sel) {
                    q.used_phys = (q.used_phys & 0xffff_ffff_0000_0000) | v as u64;
                }
            }
            0x0a4 => {
                if let Some(q) = st.queues.get_mut(sel) {
                    q.used_phys = (q.used_phys & 0xffff_ffff) | (v as u64) << 32;
                }
            }
            0x0ac => st.shm_sel = v,
            _ => {}
        }
        Ok(())
    }
}

/// The bus segment of a VirtIO device.
#[derive(Clone)]
pub struct VirtioMmio {
    pub dev: Arc<Mutex<VirtioDevice>>,
    pub name: String,
    pub start: u64,
    pub end: u64,
    pub irq: u32,
}

impl VirtioMmio {
    /// Creates the bus segment for the device.
    pub fn new(name: &str, dev: Arc<Mutex<VirtioDevice>>) -> VirtioMmio {
        let (start, end, irq) = {
            let d = dev.lock().unwrap();
            (d.state.start, d.state.end, d.state.irq)
        };
        VirtioMmio {
            dev,
            name: name.to_string(),
            start,
            end,
            irq,
        }
    }

    /// Prints the device statistics.
    pub fn stats(&self) {
        let d = self.dev.lock().unwrap();
        d.handler.device_stats(&d.state);
    }

    /// Returns the shared memory regions of the device as bus
    /// segments.
    pub fn shm_segments(&self) -> Vec<Shm> {
        let d = self.dev.lock().unwrap();
        d.state
            .shm
            .iter()
            .filter_map(|(id, data)| {
                d.state.shm_ranges.get(id).map(|(s, e)| Shm {
                    start: *s,
                    end: *e,
                    data: data.clone(),
                })
            })
            .collect()
    }

    /// Assigns physical addresses to the shared memory regions
    /// starting from base. Returns the next free address.
    pub fn assign_shm(&self, mut base: u64) -> u64 {
        let mut d = self.dev.lock().unwrap();
        let st = &mut d.state;
        let ids: Vec<u32> = st.shm.keys().copied().collect();
        for id in ids {
            let len = st.shm[&id].lock().unwrap().len() as u64;
            st.shm_ranges.insert(id, (base, base + len));
            base += len;
        }
        base
    }
}

impl Mmio for VirtioMmio {
    fn contains(&self, paddr: u64, size: u64) -> bool {
        self.start <= paddr && paddr + size <= self.end
    }

    fn load(&mut self, _now: u64, paddr: u64, size: u64) -> Result<u64, Exception> {
        self.dev.lock().unwrap().load(paddr - self.start, size)
    }

    fn store(&mut self, _now: u64, paddr: u64, size: u64, v: u64) -> Result<(), Exception> {
        self.dev.lock().unwrap().store(paddr - self.start, size, v)
    }
}

/// Returns the name of the MMIO register.
pub fn mmio_reg(ofs: u64) -> String {
    let name = match ofs {
        0x000 => "Magic",
        0x004 => "Version",
        0x008 => "DeviceID",
        0x00c => "VendorID",
        0x010 => "DeviceFeatures",
        0x014 => "DeviceFeaturesSel",
        0x020 => "DriverFeatures",
        0x024 => "DriverFeaturesSel",
        0x030 => "QueueSel",
        0x034 => "QueueNumMax",
        0x038 => "QueueNum",
        0x044 => "QueueReady",
        0x050 => "QueueNotify",
        0x060 => "InterruptStatus",
        0x064 => "InterruptACK",
        0x070 => "Status",
        0x080 => "QueueDescLow",
        0x084 => "QueueDescHigh",
        0x090 => "QueueDriverLow",
        0x094 => "QueueDriverHigh",
        0x0a0 => "QueueDeviceLow",
        0x0a4 => "QueueDeviceHigh",
        0x0ac => "SHMSel",
        0x0b0 => "SHMLenLow",
        0x0b4 => "SHMLenHigh",
        0x0b8 => "SHMBaseLow",
        0x0bc => "SHMBaseHigh",
        0x0fc => "ConfigGeneration",
        _ => return format!("0x{:03x}", ofs),
    };
    format!("{}[0x{:03x}]", name, ofs)
}
