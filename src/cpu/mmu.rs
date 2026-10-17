//! Sv39 address translation and memory accesses.

use super::{Cpu, JTLB_BANKS};
use crate::isa::{self, Exception, Mode};
use crate::memory::{self, PAGE_SIZE};
use crate::mmu::*;

/// Returns the key of the superpage of the level containing vaddr.
fn super_key(vaddr: u64, level: u32) -> u64 {
    let shift = 12 + 9 * level;
    (vaddr >> shift << shift) | level as u64
}

impl Cpu {
    /// Returns the current satp value.
    pub fn satp(&self) -> Satp {
        self.satp
    }

    /// Sets the satp configuration and flushes the TLB.
    pub fn set_satp(&mut self, satp: Satp) {
        self.satp = satp;
        self.flush_tlb();
        self.code_vpn = memory::INVALID_PAGENUM;
        self.sync_mmu_state();
    }

    /// Flushes the TLB.
    pub fn flush_tlb(&mut self) {
        // Flushing changes the generation in the TLB keys. The entries
        // are cleared only when the generation wraps around. The
        // generation never reaches 0xff, so a tag is never u64::MAX,
        // which marks unused entries.
        self.tlb_gen += 1;
        self.tlb_super.clear();
        if self.tlb_gen == 0xff {
            self.tlb_gen = 0;
            for e in self.tlb.iter_mut() {
                e.tag = u64::MAX;
            }
        }
        self.jit_tlb_clear();
        self.sync_mmu_state();
        self.new_jit_gen();
    }

    /// Flushes the translations of the virtual address (sfence.vma
    /// with an address).
    pub fn flush_tlb_page(&mut self, vaddr: u64) {
        // Superpage translations are cached as 4 KiB entries; flushing
        // a superpage flushes everything.
        if self.tlb_super.contains(&super_key(vaddr, 1))
            || self.tlb_super.contains(&super_key(vaddr, 2))
        {
            self.flush_tlb();
            return;
        }
        let idx = ((vaddr >> 12) & 0xfff) as usize;
        let e = &mut self.tlb[idx];
        if e.tag & !0xfff == vaddr & !0xfff {
            e.tag = u64::MAX;
        }
        for bank in 0..JTLB_BANKS {
            self.jtlb[bank * TLB_SIZE + idx] = JitTlbEntry::INVALID;
        }
        #[cfg(feature = "jit")]
        self.jit_flush_page(vaddr);
    }

    /// Tests if the RAM page at the offset contains JIT compiled code.
    #[inline]
    fn jit_code_page(&self, ofs: u64) -> bool {
        self.mem.is_code(ofs)
    }

    /// Invalidates the JIT jump cache entries.
    pub(super) fn new_jit_gen(&mut self) {
        self.jit_gen += 1;
        self.jit_jkey = self.jit_gen << 16 | self.fetch_key as u64;
        #[cfg(feature = "jit")]
        self.jit_clear_jcache_pages();
    }

    /// Recomputes the cached translation state after changes to the
    /// privilege mode, mstatus, or satp.
    pub(super) fn sync_mmu_state(&mut self) {
        let ms = self.mstatus;
        self.data_mode = if self.mode == Mode::M && ms.mprv() {
            ms.mpp()
        } else {
            self.mode
        };
        let bits = ((ms.sum() as u16) << 2) | ((ms.mxr() as u16) << 3) | (self.tlb_gen << 4);
        self.data_key = self.data_mode as u16 | bits;
        // JIT TLB entries filled with MXR set lose permissions when it
        // is cleared.
        if self.jtlb_mxr && !ms.mxr() {
            self.jit_tlb_clear();
        }
        self.jtlb_mxr = ms.mxr();
        self.jbank = jtlb_bank(self.data_key) * TLB_SIZE;
        self.jit_tlb_base = self.jtlb[self.jbank..].as_ptr() as u64;
        self.fetch_key = self.mode as u16 | bits;
        self.jit_jkey = self.jit_gen << 16 | self.fetch_key as u64;
    }

    /// Translates the virtual address for the access type. The access
    /// is checked against the page permissions.
    #[inline(always)]
    pub fn translate(&mut self, vaddr: u64, access: u8) -> Result<u64, Exception> {
        let (mode, key) = if access == ACCESS_EXEC {
            (self.mode, self.fetch_key)
        } else {
            (self.data_mode, self.data_key)
        };
        if mode == Mode::M || self.satp.mode() == SATP_MODE_BARE {
            return Ok(vaddr);
        }
        let vpn = vaddr >> 12;
        let tlb = &self.tlb[(vpn & 0xfff) as usize];
        if tlb.tag == TlbEntry::tag(vaddr, key) {
            let need = if access & ACCESS_WRITE != 0 {
                access | PTE_D
            } else {
                access
            };
            if tlb.flags & need == need {
                return Ok(tlb.page | (vaddr & 0xfff));
            }
        }
        self.translate_slow(vaddr, access, mode, key)
    }

    #[inline(never)]
    fn translate_slow(
        &mut self,
        vaddr: u64,
        access: u8,
        mode: Mode,
        key: u16,
    ) -> Result<u64, Exception> {
        let (page, flags, level) = self.walk_sv39(vaddr, access, mode)?;
        let offset_mask = match level {
            2 => (1 << 30) - 1,
            1 => (1 << 21) - 1,
            _ => (1 << 12) - 1,
        };
        let paddr = page | (vaddr & offset_mask);
        let page4k = paddr & !0xfff;
        if level > 0 {
            self.tlb_super.insert(super_key(vaddr, level));
        }
        let vpn = vaddr >> 12;
        self.tlb[(vpn & 0xfff) as usize] = TlbEntry {
            tag: TlbEntry::tag(vaddr, key),
            page: page4k,
            flags,
        };
        self.jit_tlb_fill(vaddr, key, page4k, flags);
        Ok(paddr)
    }

    /// Makes the JIT TLB entry of the page of lo allow the accesses to
    /// [lo, hi]. Returns false if the range crosses a page or the
    /// accesses would not run directly on RAM, for example because of a
    /// page fault. Never raises an exception.
    #[cfg(feature = "jit")]
    pub(super) fn jit_probe_range(&mut self, lo: u64, hi: u64, write: bool) -> bool {
        if lo >> 12 != hi >> 12 {
            // The range is accessible directly if both pages are, and
            // they are adjacent in the host RAM mapping.
            if hi - lo >= PAGE_SIZE {
                return false;
            }
            let hi_page = hi & !0xfff;
            if !self.jit_probe_range(lo, hi_page - 1, write)
                || !self.jit_probe_range(hi_page, hi, write)
            {
                return false;
            }
            let d_lo = self.jtlb[self.jbank + ((lo >> 12) & 0xfff) as usize].delta;
            let d_hi = self.jtlb[self.jbank + ((hi >> 12) & 0xfff) as usize].delta;
            return d_lo == d_hi && self.jtlb_host(lo, 1, write).is_some();
        }
        if self.jtlb_host(lo, hi - lo + 1, write).is_some() {
            return true;
        }
        let access = if write { ACCESS_WRITE } else { ACCESS_READ };
        if self.translate(lo, access).is_err() {
            return false;
        }
        self.jit_fill_data(lo);
        self.jtlb_host(lo, hi - lo + 1, write).is_some()
    }

    /// Refills the compiled code TLB after a successful data access or
    /// probe. A generic TLB hit does not walk or refill the JIT TLB,
    /// which can be invalidated or replaced independently.
    fn jit_fill_data(&mut self, vaddr: u64) {
        if self.data_mode == Mode::M || self.satp.mode() == SATP_MODE_BARE {
            self.jit_tlb_fill(vaddr, self.data_key, vaddr & !0xfff, PTE_R | PTE_W | PTE_D);
        } else {
            let e = self.tlb[((vaddr >> 12) & 0xfff) as usize];
            if e.tag == TlbEntry::tag(vaddr, self.data_key) {
                self.jit_tlb_fill(vaddr, self.data_key, e.page, e.flags);
            }
        }
    }

    /// Fills the compiled code TLB entry of vaddr, which maps to the
    /// physical page with the effective flags. Compiled code loads
    /// directly from readable pages in RAM, and stores directly only to
    /// dirty pages without compiled code or a store overlay.
    pub(super) fn jit_tlb_fill(&mut self, vaddr: u64, key: u16, page4k: u64, flags: u8) {
        let bank = jtlb_bank(key);
        let idx = bank * TLB_SIZE + ((vaddr >> 12) & 0xfff) as usize;
        if !self.mem.contains_range(page4k, 0x1000) {
            self.jtlb[idx] = JitTlbEntry::INVALID;
            return;
        }
        if self.jlog.len() < JLOG_MAX {
            self.jlog.push(idx as u32);
        } else {
            self.jlog_full = true;
        }
        let tag = vaddr & !0xfff;
        let ofs = self.mem.offset(page4k);
        let host = self.mem.ptr_at(ofs) as u64;
        let writable = flags & PTE_W != 0
            && flags & PTE_D != 0
            && !self.jit_code_page(ofs)
            && !self
                .overlay
                .as_ref()
                .is_some_and(|o| o.overlaps(page4k, 0x1000));
        self.jtlb[idx] = JitTlbEntry {
            tag_r: if flags & PTE_R != 0 { tag } else { u64::MAX },
            tag_w: if writable { tag } else { u64::MAX },
            delta: host.wrapping_sub(vaddr & !0xfff),
        };
    }

    /// Invalidates the JIT TLB entries.
    pub(super) fn jit_tlb_clear(&mut self) {
        if self.jlog_full {
            self.jtlb.fill(JitTlbEntry::INVALID);
        } else {
            for &i in &self.jlog {
                self.jtlb[i as usize] = JitTlbEntry::INVALID;
            }
        }
        self.jlog.clear();
        self.jlog_full = false;
    }

    fn page_fault(access: u8, vaddr: u64) -> Exception {
        let cause = if access & ACCESS_WRITE != 0 {
            isa::CAUSE_STORE_PAGE_FAULT
        } else if access & ACCESS_EXEC != 0 {
            isa::CAUSE_INST_PAGE_FAULT
        } else {
            isa::CAUSE_LOAD_PAGE_FAULT
        };
        Exception::trap(cause, vaddr)
    }

    fn access_fault(access: u8, vaddr: u64) -> Exception {
        let cause = if access & ACCESS_WRITE != 0 {
            isa::CAUSE_STORE_ACCESS_FAULT
        } else if access & ACCESS_EXEC != 0 {
            isa::CAUSE_INST_ACCESS_FAULT
        } else {
            isa::CAUSE_LOAD_ACCESS_FAULT
        };
        Exception::trap(cause, vaddr)
    }

    /// Walks the Sv39 page table. Returns the physical (super)page
    /// base address, the effective permission flags for the TLB, and
    /// the page level.
    fn walk_sv39(
        &mut self,
        vaddr: u64,
        access: u8,
        mode: Mode,
    ) -> Result<(u64, u8, u32), Exception> {
        if self.satp.mode() != SATP_MODE_SV39 {
            return Err(Exception::fatal(format!(
                "unsupported satp mode {}",
                self.satp.mode()
            )));
        }
        // The virtual address bits 63:39 must equal bit 38.
        let top = (vaddr as i64) >> 38;
        if top != 0 && top != -1 {
            return Err(Self::page_fault(access, vaddr));
        }

        let mut base = self.satp.ppn() << 12;
        for level in (0..=2u32).rev() {
            let idx = (vaddr >> (12 + 9 * level)) & 0x1ff;
            let pte_addr = base + idx * 8;
            let pte = match self.mem.load_u64(pte_addr) {
                Some(v) => Pte(v),
                None => return Err(Self::access_fault(access, vaddr)),
            };
            let flags = pte.flags();
            if !pte.valid() || (flags & PTE_W != 0 && flags & PTE_R == 0) {
                return Err(Self::page_fault(access, vaddr));
            }
            if !pte.leaf() {
                base = pte.ppn() << 12;
                continue;
            }
            return self.map_leaf(pte, pte_addr, vaddr, level, access, mode);
        }
        Err(Self::page_fault(access, vaddr))
    }

    fn map_leaf(
        &mut self,
        pte: Pte,
        pte_addr: u64,
        vaddr: u64,
        level: u32,
        access: u8,
        mode: Mode,
    ) -> Result<(u64, u8, u32), Exception> {
        let ms = self.mstatus;
        let mut flags = pte.flags();

        // Privilege checks.
        let user_page = flags & PTE_U != 0;
        match mode {
            Mode::U => {
                if !user_page {
                    return Err(Self::page_fault(access, vaddr));
                }
            }
            _ => {
                if user_page {
                    // Supervisor can never execute user pages, and
                    // can access user data only with SUM.
                    if access & ACCESS_EXEC != 0 || !ms.sum() {
                        return Err(Self::page_fault(access, vaddr));
                    }
                }
            }
        }

        // Effective read permission with MXR.
        let mut eff = flags;
        if ms.mxr() && flags & PTE_X != 0 {
            eff |= PTE_R;
        }
        if access & ACCESS_READ != 0 && eff & PTE_R == 0 {
            return Err(Self::page_fault(access, vaddr));
        }
        if access & ACCESS_WRITE != 0 && flags & PTE_W == 0 {
            return Err(Self::page_fault(access, vaddr));
        }
        if access & ACCESS_EXEC != 0 && flags & PTE_X == 0 {
            return Err(Self::page_fault(access, vaddr));
        }

        // Superpage alignment.
        let misaligned = match level {
            2 => pte.ppn1() != 0 || pte.ppn0() != 0,
            1 => pte.ppn0() != 0,
            _ => false,
        };
        if misaligned {
            return Err(Self::page_fault(access, vaddr));
        }

        let page = match level {
            2 => pte.ppn2() << 30,
            1 => pte.ppn2() << 30 | pte.ppn1() << 21,
            _ => pte.ppn() << 12,
        };

        // Update the accessed and dirty flags.
        let mut new_flags = flags | PTE_A;
        if access & ACCESS_WRITE != 0 {
            new_flags |= PTE_D;
        }
        if new_flags != flags {
            let v = (pte.0 & !0xff) | new_flags as u64;
            self.mem.store_u64(pte_addr, v);
            flags = new_flags;
            eff |= new_flags;
        }
        // Only cache write permission once the page is dirty.
        if flags & PTE_D == 0 {
            eff &= !PTE_D;
        }

        Ok((page, eff, level))
    }

    // Physical memory accesses.

    #[inline(always)]
    fn phys_load(&mut self, vaddr: u64, paddr: u64, size: u64) -> Result<u64, Exception> {
        let mem = &self.mem;
        if mem.contains_range(paddr, size) {
            let ofs = mem.offset(paddr);
            return Ok(match size {
                1 => mem.read_u8(ofs) as u64,
                2 => mem.read_u16(ofs) as u64,
                4 => mem.read_u32(ofs) as u64,
                _ => mem.read_u64(ofs),
            });
        }
        self.mmio_load(vaddr, paddr, size)
    }

    #[inline(never)]
    fn mmio_load(&mut self, vaddr: u64, paddr: u64, size: u64) -> Result<u64, Exception> {
        let now = self.time;
        match self.bus.as_mut() {
            Some(bus) => bus.load(now, paddr, size),
            None => Err(Exception::trap(isa::CAUSE_LOAD_PAGE_FAULT, vaddr)),
        }
    }

    #[inline(always)]
    fn phys_store(&mut self, vaddr: u64, paddr: u64, size: u64, v: u64) -> Result<(), Exception> {
        let mem = &self.mem;
        if mem.contains_range(paddr, size) {
            let ofs = mem.offset(paddr);
            match size {
                1 => mem.write_u8(ofs, v as u8),
                2 => mem.write_u16(ofs, v as u16),
                4 => mem.write_u32(ofs, v as u32),
                _ => mem.write_u64(ofs, v),
            }
            #[cfg(feature = "jit")]
            if mem.code_dirty() {
                self.jit_code_written();
            }
            if self.overlay.is_some() {
                return self.overlay_store(paddr, size);
            }
            return Ok(());
        }
        self.mmio_store(vaddr, paddr, size, v)
    }

    #[inline(never)]
    fn overlay_store(&mut self, paddr: u64, size: u64) -> Result<(), Exception> {
        if let Some(ov) = self.overlay.as_mut() {
            if ov.contains(paddr, size) {
                return ov.store(&self.mem, paddr);
            }
        }
        Ok(())
    }

    #[inline(never)]
    fn mmio_store(&mut self, vaddr: u64, paddr: u64, size: u64, v: u64) -> Result<(), Exception> {
        let now = self.time;
        match self.bus.as_mut() {
            Some(bus) => bus.store(now, paddr, size, v),
            None => Err(Exception::trap(isa::CAUSE_STORE_PAGE_FAULT, vaddr)),
        }
    }

    /// Returns the host address of a size byte access at vaddr that
    /// the compiled code TLB allows, if it does. The tag of the last
    /// byte is compared, so that page crossing accesses never match.
    #[inline(always)]
    fn jtlb_host(&self, vaddr: u64, size: u64, write: bool) -> Option<*mut u8> {
        let e = &self.jtlb[self.jbank + ((vaddr >> 12) & 0xfff) as usize];
        let tag = if write { e.tag_w } else { e.tag_r };
        if tag == vaddr.wrapping_add(size - 1) & !0xfff {
            Some(vaddr.wrapping_add(e.delta) as *mut u8)
        } else {
            None
        }
    }

    #[inline(always)]
    fn load_n(&mut self, vaddr: u64, size: u64) -> Result<u64, Exception> {
        if let Some(p) = self.jtlb_host(vaddr, size, false) {
            // SAFETY: JIT TLB entries map to RAM pages.
            return Ok(unsafe {
                match size {
                    1 => *p as u64,
                    2 => (p as *const u16).read_unaligned() as u64,
                    4 => (p as *const u32).read_unaligned() as u64,
                    _ => (p as *const u64).read_unaligned(),
                }
            });
        }
        self.load_slow(vaddr, size)
    }

    #[inline(never)]
    fn load_slow(&mut self, vaddr: u64, size: u64) -> Result<u64, Exception> {
        if memory::avail(vaddr, size) {
            let paddr = self.translate(vaddr, ACCESS_READ)?;
            let v = self.phys_load(vaddr, paddr, size)?;
            self.jit_fill_data(vaddr);
            return Ok(v);
        }
        self.load_split(vaddr, size)
    }

    #[inline(never)]
    fn load_split(&mut self, vaddr: u64, size: u64) -> Result<u64, Exception> {
        // Misaligned access crossing a page boundary. Translate both
        // pages before reading.
        let mut v = 0u64;
        for i in 0..size {
            let a = vaddr.wrapping_add(i);
            let paddr = self.translate(a, ACCESS_READ)?;
            v |= self.phys_load(a, paddr, 1)? << (8 * i);
        }
        Ok(v)
    }

    #[inline(always)]
    fn store_n(&mut self, vaddr: u64, size: u64, v: u64) -> Result<(), Exception> {
        if let Some(p) = self.jtlb_host(vaddr, size, true) {
            // SAFETY: JIT TLB store tags map to RAM pages without
            // compiled code or a store overlay.
            unsafe {
                match size {
                    1 => *p = v as u8,
                    2 => (p as *mut u16).write_unaligned(v as u16),
                    4 => (p as *mut u32).write_unaligned(v as u32),
                    _ => (p as *mut u64).write_unaligned(v),
                }
            }
            return Ok(());
        }
        self.store_slow(vaddr, size, v)
    }

    #[inline(never)]
    fn store_slow(&mut self, vaddr: u64, size: u64, v: u64) -> Result<(), Exception> {
        if memory::avail(vaddr, size) {
            let paddr = self.translate(vaddr, ACCESS_WRITE)?;
            self.phys_store(vaddr, paddr, size, v)?;
            self.jit_fill_data(vaddr);
            return Ok(());
        }
        self.store_split(vaddr, size, v)
    }

    #[inline(never)]
    fn store_split(&mut self, vaddr: u64, size: u64, v: u64) -> Result<(), Exception> {
        // Check that both pages are writable before modifying memory.
        self.translate(vaddr, ACCESS_WRITE)?;
        self.translate(vaddr.wrapping_add(size - 1), ACCESS_WRITE)?;
        for i in 0..size {
            let a = vaddr.wrapping_add(i);
            let paddr = self.translate(a, ACCESS_WRITE)?;
            self.phys_store(a, paddr, 1, v >> (8 * i))?;
        }
        Ok(())
    }

    /// Loads a 8-bit value from the virtual address.
    #[inline(always)]
    pub fn load8(&mut self, vaddr: u64) -> Result<u8, Exception> {
        self.load_n(vaddr, 1).map(|v| v as u8)
    }

    /// Loads a 16-bit value from the virtual address.
    #[inline(always)]
    pub fn load16(&mut self, vaddr: u64) -> Result<u16, Exception> {
        self.load_n(vaddr, 2).map(|v| v as u16)
    }

    /// Loads a 32-bit value from the virtual address.
    #[inline(always)]
    pub fn load32(&mut self, vaddr: u64) -> Result<u32, Exception> {
        self.load_n(vaddr, 4).map(|v| v as u32)
    }

    /// Loads a 64-bit value from the virtual address.
    #[inline(always)]
    pub fn load64(&mut self, vaddr: u64) -> Result<u64, Exception> {
        self.load_n(vaddr, 8)
    }

    /// Stores a 8-bit value to the virtual address.
    #[inline(always)]
    pub fn store8(&mut self, vaddr: u64, v: u8) -> Result<(), Exception> {
        self.store_n(vaddr, 1, v as u64)
    }

    /// Stores a 16-bit value to the virtual address.
    #[inline(always)]
    pub fn store16(&mut self, vaddr: u64, v: u16) -> Result<(), Exception> {
        self.store_n(vaddr, 2, v as u64)
    }

    /// Stores a 32-bit value to the virtual address.
    #[inline(always)]
    pub fn store32(&mut self, vaddr: u64, v: u32) -> Result<(), Exception> {
        self.store_n(vaddr, 4, v as u64)
    }

    /// Stores a 64-bit value to the virtual address.
    #[inline(always)]
    pub fn store64(&mut self, vaddr: u64, v: u64) -> Result<(), Exception> {
        self.store_n(vaddr, 8, v)
    }

    /// Reads a NUL-terminated string from the virtual address.
    pub fn user_cstring(&mut self, mut vaddr: u64) -> Result<String, Exception> {
        let mut data = Vec::new();
        loop {
            let paddr = self.translate(vaddr, ACCESS_READ)?;
            let l = PAGE_SIZE - paddr % PAGE_SIZE;
            for i in 0..l {
                let b = self
                    .mem
                    .load_u8(paddr + i)
                    .ok_or_else(|| Exception::trap(isa::CAUSE_LOAD_ACCESS_FAULT, vaddr + i))?;
                if b == 0 {
                    return Ok(String::from_utf8_lossy(&data).to_string());
                }
                data.push(b);
            }
            vaddr += l;
        }
    }

    /// Copies data from the virtual address into buf.
    pub fn copy_from_user(&mut self, mut vaddr: u64, mut buf: &mut [u8]) -> Result<(), Exception> {
        while !buf.is_empty() {
            let l = ((PAGE_SIZE - vaddr % PAGE_SIZE) as usize).min(buf.len());
            let paddr = self.translate(vaddr, ACCESS_READ)?;
            if !self.mem.contains_range(paddr, l as u64) {
                return Err(Exception::trap(isa::CAUSE_LOAD_ACCESS_FAULT, vaddr));
            }
            let (head, tail) = buf.split_at_mut(l);
            self.mem.read_bytes(self.mem.offset(paddr), head);
            buf = tail;
            vaddr += l as u64;
        }
        Ok(())
    }

    /// Copies data to the virtual address.
    pub fn copy_to_user(&mut self, mut vaddr: u64, mut data: &[u8]) -> Result<(), Exception> {
        while !data.is_empty() {
            let l = ((PAGE_SIZE - vaddr % PAGE_SIZE) as usize).min(data.len());
            let paddr = self.translate(vaddr, ACCESS_WRITE)?;
            if self.mem.contains_range(paddr, l as u64) {
                self.mem.write_bytes(self.mem.offset(paddr), &data[..l]);
            } else {
                for (i, b) in data[..l].iter().enumerate() {
                    self.mmio_store(vaddr + i as u64, paddr + i as u64, 1, *b as u64)?;
                }
            }
            data = &data[l..];
            vaddr += l as u64;
        }
        Ok(())
    }
}

/// Maximum length of the logs of filled JIT TLB entries; longer logs
/// clear everything.
const JLOG_MAX: usize = 2048;

/// Returns the JIT TLB bank of the TLB key: the bank of the data mode,
/// with a separate supervisor bank for SUM.
#[inline]
fn jtlb_bank(key: u16) -> usize {
    let sum = key & 1 << 2 != 0;
    match Mode::from_bits(key as u64 & 3) {
        Mode::U => 0,
        Mode::S if sum => 2,
        Mode::S => 1,
        _ => 3,
    }
}
