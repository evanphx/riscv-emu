//! Sv39 address translation and memory accesses.

use super::Cpu;
use crate::isa::{self, Exception, Mode};
use crate::memory::{self, PAGE_SIZE};
use crate::mmu::*;

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
        for e in self.tlb.iter_mut() {
            e.tag = u64::MAX;
        }
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
        let bits = ((ms.sum() as u8) << 2) | ((ms.mxr() as u8) << 3);
        self.data_key = self.data_mode as u8 | bits;
        self.fetch_key = self.mode as u8 | bits;
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
        if tlb.tag == TlbEntry::tag(vpn, key) {
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
        key: u8,
    ) -> Result<u64, Exception> {
        let (page, flags, level) = self.walk_sv39(vaddr, access, mode)?;
        let offset_mask = match level {
            2 => (1 << 30) - 1,
            1 => (1 << 21) - 1,
            _ => (1 << 12) - 1,
        };
        let paddr = page | (vaddr & offset_mask);
        let page4k = paddr & !0xfff;
        let vpn = vaddr >> 12;
        let (host_delta, host_flags) = if self.mem.contains_range(page4k, 0x1000) {
            let host = self.mem.ptr_at(self.mem.offset(page4k)) as u64;
            (host.wrapping_sub(vaddr & !0xfff), flags)
        } else {
            (0, 0)
        };
        self.tlb[(vpn & 0xfff) as usize] = TlbEntry {
            tag: TlbEntry::tag(vpn, key),
            page: page4k,
            host_delta,
            flags,
            host_flags,
        };
        Ok(paddr)
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
            if !self.jit_code_pages.is_empty() {
                self.jit_code_write(ofs);
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

    #[inline(always)]
    fn load_n(&mut self, vaddr: u64, size: u64) -> Result<u64, Exception> {
        if memory::avail(vaddr, size) {
            let paddr = self.translate(vaddr, ACCESS_READ)?;
            return self.phys_load(vaddr, paddr, size);
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
        if memory::avail(vaddr, size) {
            let paddr = self.translate(vaddr, ACCESS_WRITE)?;
            return self.phys_store(vaddr, paddr, size, v);
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
