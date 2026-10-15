//! Guest physical memory.
//!
//! Guest RAM is shared between the CPU thread and device worker
//! threads (DMA). Like real hardware, concurrent accesses to the same
//! bytes are not synchronized by the memory itself; the guest
//! coordinates them through the device protocols (for example VirtIO
//! descriptor ownership). All accessors therefore go through raw
//! pointers with unaligned loads and stores.

use std::sync::atomic::{AtomicUsize, Ordering};

/// System page size in bytes.
pub const PAGE_SIZE: u64 = 4096;

/// RAM base address.
pub const RAM_BASE: u64 = 0x8000_0000;

/// An invalid page number. On Sv39 no valid page number can have all
/// 64 bits set.
pub const INVALID_PAGENUM: u64 = u64::MAX;

/// Tests if n bytes starting from addr fit in addr's page.
#[inline(always)]
pub fn avail(addr: u64, n: u64) -> bool {
    (addr & 0xfff) + n <= 0x1000
}

/// Returns the page number of the address.
#[inline(always)]
pub fn page(addr: u64) -> u64 {
    addr >> 12
}

/// Returns the offset of the address within its page.
#[inline(always)]
pub fn page_offset(addr: u64) -> u64 {
    addr & 0xfff
}

/// Errors returned by memory allocation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MemError {
    InvalidAddr,
    OutOfMemory,
}

impl std::fmt::Display for MemError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            MemError::InvalidAddr => f.write_str("invalid address"),
            MemError::OutOfMemory => f.write_str("out of memory"),
        }
    }
}

impl std::error::Error for MemError {}

/// Guest physical RAM.
pub struct Memory {
    ptr: *mut u8,
    len: usize,
    pub ram_base: u64,
    pub ram_end: u64,
    num_pages: usize,
    next_page: AtomicUsize,
}

// SAFETY: guest memory is intentionally shared between the CPU and
// device threads; see the module documentation.
unsafe impl Send for Memory {}
unsafe impl Sync for Memory {}

impl Drop for Memory {
    fn drop(&mut self) {
        // SAFETY: ptr and len come from Box::into_raw in new().
        unsafe {
            drop(Box::from_raw(std::ptr::slice_from_raw_parts_mut(
                self.ptr, self.len,
            )));
        }
    }
}

impl Memory {
    /// Creates a new RAM of size bytes at the physical address base.
    pub fn new(base: u64, size: u64) -> Memory {
        assert!(
            size.is_multiple_of(PAGE_SIZE),
            "memory size is not multiple of page size"
        );
        let buf = vec![0u8; size as usize].into_boxed_slice();
        let len = buf.len();
        let ptr = Box::into_raw(buf) as *mut u8;
        Memory {
            ptr,
            len,
            ram_base: base,
            ram_end: base + size,
            num_pages: (size / PAGE_SIZE) as usize,
            next_page: AtomicUsize::new(0),
        }
    }

    /// RAM size in bytes.
    #[inline(always)]
    pub fn size(&self) -> u64 {
        self.len as u64
    }

    /// Tests if the physical address is in RAM.
    #[inline(always)]
    pub fn contains(&self, addr: u64) -> bool {
        self.ram_base <= addr && addr < self.ram_end
    }

    /// Tests if the physical range [addr, addr+n) is in RAM.
    #[inline(always)]
    pub fn contains_range(&self, addr: u64, n: u64) -> bool {
        addr >= self.ram_base && n <= self.len as u64 && addr - self.ram_base <= self.len as u64 - n
    }

    /// Returns the RAM offset of the physical address.
    #[inline(always)]
    pub fn offset(&self, paddr: u64) -> u64 {
        paddr.wrapping_sub(self.ram_base)
    }

    /// Allocates the next free page and returns its page number.
    pub fn alloc_page(&self) -> Result<u64, MemError> {
        let n = self.next_page.fetch_add(1, Ordering::SeqCst);
        if n >= self.num_pages {
            self.next_page.fetch_sub(1, Ordering::SeqCst);
            return Err(MemError::OutOfMemory);
        }
        Ok(self.ram_base / PAGE_SIZE + n as u64)
    }

    /// Returns the RAM offset of the page number, or an error if the
    /// page is outside RAM.
    #[inline(always)]
    pub fn page_offset_of(&self, num: u64) -> Result<u64, MemError> {
        let base = self.ram_base / PAGE_SIZE;
        if num < base || num >= base + self.num_pages as u64 {
            return Err(MemError::InvalidAddr);
        }
        Ok((num - base) * PAGE_SIZE)
    }

    /// Returns a raw pointer to the RAM offset.
    #[inline(always)]
    pub fn ptr_at(&self, ofs: u64) -> *mut u8 {
        debug_assert!(ofs < self.len as u64);
        // SAFETY: callers ensure that ofs is inside RAM.
        unsafe { self.ptr.add(ofs as usize) }
    }

    // The following accessors take RAM offsets. They panic if the
    // access is outside RAM.

    #[inline(always)]
    fn check(&self, ofs: u64, n: u64) {
        if ofs.checked_add(n).is_none_or(|end| end > self.len as u64) {
            panic!("memory access out of range: ofs={:x}, n={}", ofs, n);
        }
    }

    #[inline(always)]
    pub fn read_u8(&self, ofs: u64) -> u8 {
        self.check(ofs, 1);
        unsafe { *self.ptr.add(ofs as usize) }
    }

    #[inline(always)]
    pub fn read_u16(&self, ofs: u64) -> u16 {
        self.check(ofs, 2);
        unsafe { (self.ptr.add(ofs as usize) as *const u16).read_unaligned() }
    }

    #[inline(always)]
    pub fn read_u32(&self, ofs: u64) -> u32 {
        self.check(ofs, 4);
        unsafe { (self.ptr.add(ofs as usize) as *const u32).read_unaligned() }
    }

    #[inline(always)]
    pub fn read_u64(&self, ofs: u64) -> u64 {
        self.check(ofs, 8);
        unsafe { (self.ptr.add(ofs as usize) as *const u64).read_unaligned() }
    }

    #[inline(always)]
    pub fn write_u8(&self, ofs: u64, v: u8) {
        self.check(ofs, 1);
        unsafe { *self.ptr.add(ofs as usize) = v }
    }

    #[inline(always)]
    pub fn write_u16(&self, ofs: u64, v: u16) {
        self.check(ofs, 2);
        unsafe { (self.ptr.add(ofs as usize) as *mut u16).write_unaligned(v) }
    }

    #[inline(always)]
    pub fn write_u32(&self, ofs: u64, v: u32) {
        self.check(ofs, 4);
        unsafe { (self.ptr.add(ofs as usize) as *mut u32).write_unaligned(v) }
    }

    #[inline(always)]
    pub fn write_u64(&self, ofs: u64, v: u64) {
        self.check(ofs, 8);
        unsafe { (self.ptr.add(ofs as usize) as *mut u64).write_unaligned(v) }
    }

    /// Copies RAM starting from offset ofs into buf.
    pub fn read_bytes(&self, ofs: u64, buf: &mut [u8]) {
        self.check(ofs, buf.len() as u64);
        unsafe {
            std::ptr::copy_nonoverlapping(self.ptr.add(ofs as usize), buf.as_mut_ptr(), buf.len())
        }
    }

    /// Copies data into RAM starting from offset ofs.
    pub fn write_bytes(&self, ofs: u64, data: &[u8]) {
        self.check(ofs, data.len() as u64);
        unsafe {
            std::ptr::copy_nonoverlapping(data.as_ptr(), self.ptr.add(ofs as usize), data.len())
        }
    }

    /// Fills RAM starting from offset ofs with n copies of v.
    pub fn fill(&self, ofs: u64, n: u64, v: u8) {
        self.check(ofs, n);
        unsafe { std::ptr::write_bytes(self.ptr.add(ofs as usize), v, n as usize) }
    }

    /// Returns a mutable slice of guest RAM at the physical address.
    ///
    /// # Safety
    ///
    /// Guest memory is shared with other threads. The caller must
    /// ensure that the guest has handed the range over to the caller
    /// (for example through a VirtIO descriptor) so that no other
    /// Rust reference to the same bytes is used concurrently.
    #[allow(clippy::mut_from_ref)]
    pub unsafe fn guest_slice_mut(&self, paddr: u64, len: u64) -> Option<&mut [u8]> {
        if !self.contains_range(paddr, len) {
            return None;
        }
        let ofs = self.offset(paddr) as usize;
        Some(std::slice::from_raw_parts_mut(
            self.ptr.add(ofs),
            len as usize,
        ))
    }

    /// Returns a slice of guest RAM at the physical address.
    ///
    /// # Safety
    ///
    /// See [`Memory::guest_slice_mut`].
    pub unsafe fn guest_slice(&self, paddr: u64, len: u64) -> Option<&[u8]> {
        if !self.contains_range(paddr, len) {
            return None;
        }
        let ofs = self.offset(paddr) as usize;
        Some(std::slice::from_raw_parts(self.ptr.add(ofs), len as usize))
    }

    // Physical address accessors returning None for addresses outside
    // RAM.

    pub fn load_u8(&self, paddr: u64) -> Option<u8> {
        self.contains_range(paddr, 1)
            .then(|| self.read_u8(self.offset(paddr)))
    }

    pub fn load_u16(&self, paddr: u64) -> Option<u16> {
        self.contains_range(paddr, 2)
            .then(|| self.read_u16(self.offset(paddr)))
    }

    pub fn load_u32(&self, paddr: u64) -> Option<u32> {
        self.contains_range(paddr, 4)
            .then(|| self.read_u32(self.offset(paddr)))
    }

    pub fn load_u64(&self, paddr: u64) -> Option<u64> {
        self.contains_range(paddr, 8)
            .then(|| self.read_u64(self.offset(paddr)))
    }

    pub fn store_u8(&self, paddr: u64, v: u8) -> Option<()> {
        self.contains_range(paddr, 1)
            .then(|| self.write_u8(self.offset(paddr), v))
    }

    pub fn store_u16(&self, paddr: u64, v: u16) -> Option<()> {
        self.contains_range(paddr, 2)
            .then(|| self.write_u16(self.offset(paddr), v))
    }

    pub fn store_u32(&self, paddr: u64, v: u32) -> Option<()> {
        self.contains_range(paddr, 4)
            .then(|| self.write_u32(self.offset(paddr), v))
    }

    pub fn store_u64(&self, paddr: u64, v: u64) -> Option<()> {
        self.contains_range(paddr, 8)
            .then(|| self.write_u64(self.offset(paddr), v))
    }

    /// Prints the printable strings of the RAM.
    pub fn strings(&self) {
        println!("*** strings ***");
        let mut out = String::new();
        for i in 0..self.len as u64 {
            let b = self.read_u8(i);
            if matches!(b, b'\t' | b'\n' | b'\r') || (b' '..=b'~').contains(&b) {
                out.push(b as char);
            }
        }
        println!("{}", out);
        println!("*** strings ***");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn get_put() {
        let mem = Memory::new(RAM_BASE, 2 * PAGE_SIZE);
        mem.write_u64(3, 0x0102030405060708);
        assert_eq!(mem.read_u64(3), 0x0102030405060708);
        assert_eq!(mem.read_u32(3), 0x05060708);
        assert_eq!(mem.read_u16(5), 0x0506);
        assert_eq!(mem.read_u8(10), 0x01);
        assert_eq!(mem.load_u64(RAM_BASE + 3), Some(0x0102030405060708));
        assert_eq!(mem.load_u64(RAM_BASE + 2 * PAGE_SIZE - 4), None);
        assert_eq!(mem.load_u8(RAM_BASE - 1), None);
    }

    #[test]
    fn alloc() {
        let mem = Memory::new(RAM_BASE, 2 * PAGE_SIZE);
        assert_eq!(mem.alloc_page(), Ok(RAM_BASE / PAGE_SIZE));
        assert_eq!(mem.alloc_page(), Ok(RAM_BASE / PAGE_SIZE + 1));
        assert_eq!(mem.alloc_page(), Err(MemError::OutOfMemory));
    }
}
