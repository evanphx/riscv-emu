//! Virtual memory areas.

use super::Kernel;
use crate::isa;
use crate::mmu::{ACCESS_EXEC, ACCESS_READ, ACCESS_WRITE, PTE_R, PTE_U, PTE_V, PTE_W, PTE_X};
use std::fmt;
use std::fs::File;
use std::sync::Arc;

/// A virtual memory area [start, end) with its access permissions
/// and optional backing file.
#[derive(Clone)]
pub struct Vma {
    pub start: u64,
    pub end: u64,

    pub source: Option<Arc<File>>,
    pub offset: u64,

    /// Access permissions (mmu::ACCESS_*).
    pub prot: u8,
}

impl Vma {
    /// Returns the page table flags for the area's pages.
    pub fn page_table_flags(&self) -> u8 {
        let mut flags = 0;
        if self.prot & ACCESS_READ != 0 {
            flags |= PTE_R;
        }
        if self.prot & ACCESS_WRITE != 0 {
            flags |= PTE_W;
        }
        if self.prot & ACCESS_EXEC != 0 {
            flags |= PTE_X;
        }
        flags | PTE_U | PTE_V
    }

    /// Returns a copy of the area without its backing file.
    fn clone_area(&self) -> Vma {
        Vma {
            start: self.start,
            end: self.end,
            source: None,
            offset: 0,
            prot: self.prot,
        }
    }

    /// Tests if the area contains the address.
    pub fn contains(&self, addr: u64) -> bool {
        self.start <= addr && addr < self.end
    }

    /// Tests if the area permissions allow the access that caused the
    /// page fault.
    pub fn satisfies(&self, cause: u64) -> bool {
        match cause {
            isa::CAUSE_LOAD_PAGE_FAULT => self.prot & ACCESS_READ != 0,
            isa::CAUSE_STORE_PAGE_FAULT => self.prot & ACCESS_WRITE != 0,
            isa::CAUSE_INST_PAGE_FAULT => self.prot & ACCESS_EXEC != 0,
            _ => false,
        }
    }

    fn same(&self, o: &Vma) -> bool {
        self.start == o.start && self.end == o.end
    }

    fn overlaps(&self, o: &Vma) -> bool {
        self.start < o.end && self.end > o.start
    }

    fn covers(&self, o: &Vma) -> bool {
        self.start <= o.start && self.end >= o.end
    }

    fn same_source(&self, o: &Vma) -> bool {
        match (&self.source, &o.source) {
            (None, None) => true,
            (Some(a), Some(b)) => Arc::ptr_eq(a, b),
            _ => false,
        }
    }
}

impl fmt::Display for Vma {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let x = if self.prot & ACCESS_EXEC != 0 {
            'X'
        } else {
            '.'
        };
        let w = if self.prot & ACCESS_WRITE != 0 {
            'W'
        } else {
            '.'
        };
        let r = if self.prot & ACCESS_READ != 0 {
            'R'
        } else {
            '.'
        };
        write!(f, "{:10x}:{:10x} {}{}{}", self.start, self.end, x, w, r)?;
        if let Some(src) = &self.source {
            write!(f, " <- {:p} @ {}", Arc::as_ptr(src), self.offset)?;
        }
        Ok(())
    }
}

impl Kernel {
    /// Prints the virtual memory areas.
    pub fn print_vma(&self) {
        println!("VMA:");
        for (i, vma) in self.vma.iter().enumerate() {
            println!("{:3}: {}", i, vma);
        }
    }

    /// Adds the area [start, end). The new area replaces the
    /// overlapping parts of the existing areas.
    pub fn add_vma(
        &mut self,
        start: u64,
        end: u64,
        prot: u8,
        source: Option<Arc<File>>,
        offset: u64,
    ) {
        let mut vma = Vma {
            start,
            end,
            source,
            offset,
            prot,
        };

        let mut i = 0;
        while i < self.vma.len() {
            let o = &self.vma[i];

            if vma.same(o) {
                self.vma[i] = vma;
                return self.compact_vma();
            }
            if vma.covers(o) {
                self.vma.remove(i);
                continue;
            }
            if o.covers(&vma) {
                // New:      |nnn|
                // Old: |oooooooooooo|
                // => : |oooo|nnn|ooo|
                let mut tail = o.clone_area();
                tail.start = vma.end;
                self.vma[i].end = vma.start;
                self.vma.insert(i + 1, vma);
                self.vma.insert(i + 2, tail);
                return self.compact_vma();
            }

            if vma.overlaps(o) {
                if vma.start < o.start {
                    // New: |nnnnnnnnnn|
                    // Old:       |ooooooooo|
                    // => : |nnnnnnnnnn|oooo|
                    self.vma[i].start = vma.end;
                    self.vma.insert(i, vma);
                    return self.compact_vma();
                }
                // New:      |nnnnnNNNNN|
                // Old: |ooooooooo|
                // => : |oooo|nnnn|, insert |NNNNN|
                let mut tail = vma.clone_area();
                tail.end = o.end;
                self.vma[i].end = vma.start;
                vma.start = tail.end;
                self.vma.insert(i + 1, tail);
                i += 2;
            } else if vma.start < o.start {
                // Insert before o.
                self.vma.insert(i, vma);
                return self.compact_vma();
            } else {
                i += 1;
            }
        }

        self.vma.push(vma);
        self.compact_vma();
    }

    /// Removes empty areas and merges adjacent compatible areas.
    fn compact_vma(&mut self) {
        let mut prev = 0;
        let mut i = 1;
        while i < self.vma.len() {
            let vma = &self.vma[i];
            let p = &self.vma[prev];

            if vma.start == vma.end {
                self.vma.remove(i);
                continue;
            }
            if vma.start == p.end
                && vma.prot == p.prot
                && vma.same_source(p)
                && vma.start.wrapping_sub(p.start) == vma.offset.wrapping_sub(p.offset)
            {
                self.vma[prev].end = self.vma[i].end;
                self.vma.remove(i);
            } else {
                prev = i;
                i += 1;
            }
        }
        self.check_vma();
    }

    /// Verifies that the areas are non-empty and sorted.
    fn check_vma(&self) {
        for w in self.vma.windows(2) {
            if w[1].start >= w[1].end {
                panic!("empty VMA");
            }
            if w[1].start < w[0].end {
                panic!("not sorted");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn vma() {
        let tests = [
            (0x2000, 0x3000, ACCESS_READ),
            (0x1000, 0x2000, ACCESS_READ),
            (0x1000, 0x3000, ACCESS_READ),
            (0x1000, 0x3000, ACCESS_READ | ACCESS_WRITE),
            (0x1500, 0x2000, ACCESS_EXEC),
            (0x0500, 0x1800, ACCESS_READ),
            (0x2500, 0x5000, ACCESS_READ),
        ];
        let mut kern = Kernel::default();
        for (start, end, prot) in tests {
            kern.add_vma(start, end, prot, None, 0);
        }

        let result: Vec<String> = kern.vma.iter().map(|v| v.to_string()).collect();
        assert_eq!(
            result,
            [
                "       500:      1800 ..R",
                "      1800:      2000 X..",
                "      2000:      2500 .WR",
                "      2500:      3000 ..R",
                "      3000:      5000 ..R",
            ]
        );
    }
}
