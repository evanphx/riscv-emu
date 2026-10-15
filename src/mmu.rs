//! Memory management unit types: SATP, page table entries, the TLB,
//! and the memory-mapped I/O device interface. The address
//! translation itself is implemented by the CPU (see `cpu::mmu`).

use crate::isa::Exception;
use crate::memory::Memory;
use std::fmt;

/// Memory access types.
pub const ACCESS_NONE: u8 = 0;
pub const ACCESS_READ: u8 = PTE_R;
pub const ACCESS_WRITE: u8 = PTE_W;
pub const ACCESS_EXEC: u8 = PTE_X;

/// SATP modes.
pub const SATP_MODE_BARE: u64 = 0;
pub const SATP_MODE_SV39: u64 = 8;
pub const SATP_MODE_SV48: u64 = 9;
pub const SATP_MODE_SV57: u64 = 10;

/// Page table entry flags.
pub const PTE_V: u8 = 1 << 0;
pub const PTE_R: u8 = 1 << 1;
pub const PTE_W: u8 = 1 << 2;
pub const PTE_X: u8 = 1 << 3;
pub const PTE_U: u8 = 1 << 4;
pub const PTE_G: u8 = 1 << 5;
pub const PTE_A: u8 = 1 << 6;
pub const PTE_D: u8 = 1 << 7;

/// A memory-mapped I/O device.
pub trait Mmio: Send {
    /// Tests if the device covers the physical range [paddr, paddr+size).
    fn contains(&self, paddr: u64, size: u64) -> bool;

    /// Loads a size byte value from the physical address. The `now`
    /// argument is the current CPU time.
    fn load(&mut self, now: u64, paddr: u64, size: u64) -> Result<u64, Exception>;

    /// Stores a size byte value to the physical address.
    fn store(&mut self, now: u64, paddr: u64, size: u64, v: u64) -> Result<(), Exception>;

    /// Halts the device when the CPU stops.
    fn halt(&mut self) -> Result<(), String> {
        Ok(())
    }
}

/// An overlay intercepts stores to RAM addresses (for example the
/// HTIF tohost/fromhost registers).
pub trait Overlay: Send {
    fn contains(&self, paddr: u64, size: u64) -> bool;
    fn store(&mut self, mem: &Memory, paddr: u64) -> Result<(), Exception>;
}

/// Supervisor Address Translation and Protection register.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Satp(pub u64);

impl Satp {
    pub fn new(mode: u64, ppn: u64) -> Satp {
        Satp(mode << 60 | (ppn & 0xfff_ffff_ffff))
    }

    pub fn mode(self) -> u64 {
        self.0 >> 60
    }

    pub fn asid(self) -> u16 {
        (self.0 >> 44) as u16
    }

    pub fn ppn(self) -> u64 {
        self.0 & 0xfff_ffff_ffff
    }
}

impl fmt::Display for Satp {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "mode={}, asid={:x}, ppn={:x}",
            self.mode(),
            self.asid(),
            self.ppn()
        )
    }
}

/// Formats PTE flags in the DAGUXWRV format.
pub fn flags_string(flags: u8) -> String {
    let names = ['D', 'A', 'G', 'U', 'X', 'W', 'R', 'V'];
    let mut s = String::new();
    for (i, n) in names.iter().enumerate() {
        if flags & (1 << (7 - i)) != 0 {
            s.push(*n);
        } else {
            s.push('.');
        }
    }
    s
}

/// Page table entry.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Pte(pub u64);

impl Pte {
    pub fn make(ppn: u64, flags: u8) -> Pte {
        Pte(ppn << 10 | flags as u64)
    }

    #[inline]
    pub fn flags(self) -> u8 {
        self.0 as u8
    }

    #[inline]
    pub fn valid(self) -> bool {
        self.flags() & PTE_V != 0
    }

    #[inline]
    pub fn leaf(self) -> bool {
        self.flags() & (PTE_R | PTE_W | PTE_X) != 0
    }

    #[inline]
    pub fn ppn(self) -> u64 {
        (self.0 >> 10) & 0xfff_ffff_ffff
    }

    pub fn ppn0(self) -> u64 {
        self.ppn() & 0x1ff
    }

    pub fn ppn1(self) -> u64 {
        (self.ppn() >> 9) & 0x1ff
    }

    pub fn ppn2(self) -> u64 {
        self.ppn() >> 18
    }
}

impl fmt::Display for Pte {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{:011x}/{:03x}/{:03x},{:02b},{}",
            self.ppn2(),
            self.ppn1(),
            self.ppn0(),
            (self.0 >> 8) & 0b11,
            flags_string(self.flags())
        )
    }
}

/// Number of TLB entries.
pub const TLB_SIZE: usize = 4096;

/// A Translation Lookaside Buffer entry.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TlbEntry {
    /// Virtual page number. `u64::MAX` marks an invalid entry.
    pub vpn: u64,
    /// Physical base address of the (super)page.
    pub page: u64,
    /// Offset mask of the (super)page.
    pub offset_mask: u64,
    /// Effective PTE flags.
    pub flags: u8,
    /// Privilege key: mode | SUM << 2 | MXR << 3.
    pub key: u8,
}

impl Default for TlbEntry {
    fn default() -> Self {
        TlbEntry {
            vpn: u64::MAX,
            page: 0,
            offset_mask: 0,
            flags: 0,
            key: 0,
        }
    }
}

/// Shared memory region exposed to the guest through MMIO.
pub struct Shm {
    pub start: u64,
    pub end: u64,
    pub data: std::sync::Arc<std::sync::Mutex<Vec<u8>>>,
}

impl Mmio for Shm {
    fn contains(&self, paddr: u64, size: u64) -> bool {
        self.start <= paddr && paddr + size <= self.end
    }

    fn load(&mut self, _now: u64, paddr: u64, size: u64) -> Result<u64, Exception> {
        let data = self.data.lock().unwrap();
        let o = (paddr - self.start) as usize;
        let mut v = 0u64;
        for i in 0..size as usize {
            v |= (*data.get(o + i).unwrap_or(&0) as u64) << (8 * i);
        }
        Ok(v)
    }

    fn store(&mut self, _now: u64, paddr: u64, size: u64, v: u64) -> Result<(), Exception> {
        let mut data = self.data.lock().unwrap();
        let o = (paddr - self.start) as usize;
        for i in 0..size as usize {
            if let Some(b) = data.get_mut(o + i) {
                *b = (v >> (8 * i)) as u8;
            }
        }
        Ok(())
    }
}

/// Adds the mapping vpage -> ppage into the Sv39 page table of satp,
/// allocating intermediate page table pages from mem.
pub fn set_map_sv39(
    mem: &Memory,
    satp: Satp,
    vpage: u64,
    ppage: u64,
    flags: u8,
) -> Result<(), String> {
    if satp.mode() != SATP_MODE_SV39 {
        return Err(format!("invalid page-table mode: {}", satp.mode()));
    }
    let flags = flags | PTE_V;
    let mut base = satp.ppn() << 12;

    for level in (1..=2).rev() {
        let idx = (vpage >> (9 * level)) & 0x1ff;
        let pte_addr = base + idx * 8;
        let pte = Pte(mem.read_u64(mem.offset(pte_addr)));
        if pte.valid() {
            if pte.leaf() {
                return Err("superpage exists".to_string());
            }
            base = pte.ppn() << 12;
        } else {
            let new_page = mem.alloc_page().map_err(|e| e.to_string())?;
            let ofs = mem.page_offset_of(new_page).map_err(|e| e.to_string())?;
            mem.fill(ofs, crate::memory::PAGE_SIZE, 0);
            mem.write_u64(mem.offset(pte_addr), Pte::make(new_page, PTE_V).0);
            base = new_page << 12;
        }
    }

    let idx = vpage & 0x1ff;
    let pte_addr = base + idx * 8;
    let pte = Pte(mem.read_u64(mem.offset(pte_addr)));
    if pte.valid() {
        return Err(format!("mapping already exists: {}", pte));
    }
    mem.write_u64(mem.offset(pte_addr), Pte::make(ppage, flags).0);
    Ok(())
}

/// Returns the Sv39 leaf PTE address for vpage, if mapped by a 4 KiB
/// page.
pub fn lookup_sv39(mem: &Memory, satp: Satp, vpage: u64) -> Option<u64> {
    let mut base = satp.ppn() << 12;
    for level in (0..=2).rev() {
        let idx = (vpage >> (9 * level)) & 0x1ff;
        let pte_addr = base + idx * 8;
        let pte = Pte(mem.load_u64(pte_addr)?);
        if !pte.valid() {
            return None;
        }
        if pte.leaf() {
            return if level == 0 { Some(pte_addr) } else { None };
        }
        base = pte.ppn() << 12;
    }
    None
}

/// Dumps the Sv39 page table of satp.
pub fn dump(mem: &Memory, satp: Satp) -> Result<(), String> {
    let mode = satp.mode();
    if mode == SATP_MODE_BARE {
        println!("bare mode");
        return Ok(());
    } else if mode != SATP_MODE_SV39 {
        return Err(format!("unsupported mode {}", mode));
    }
    dump_level(mem, satp.ppn() << 12, 0, 2);
    Ok(())
}

fn dump_level(mem: &Memory, root: u64, vpage: u64, level: u32) {
    for i in 0..512u64 {
        let pte = match mem.load_u64(root + i * 8) {
            Some(v) => Pte(v),
            None => return,
        };
        if !pte.valid() {
            continue;
        }
        if level == 0 || pte.leaf() {
            println!("pte: {}: {:08x} => {:014x}", pte, vpage + i, pte.ppn());
        } else {
            dump_level(mem, pte.ppn() << 12, vpage | (i << (9 * level)), level - 1);
        }
    }
}
