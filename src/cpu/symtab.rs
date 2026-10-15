//! Kernel symbol table (System.map).

use std::io::BufRead;

/// Symbol table entry.
#[derive(Clone, Debug)]
pub struct SymEntry {
    pub start: u64,
    pub end: u64,
    pub typ: char,
    pub name: String,
}

impl SymEntry {
    pub fn contains(&self, addr: u64) -> bool {
        self.start <= addr && addr < self.end
    }
}

/// Symbol table loaded from a Linux System.map file.
#[derive(Clone, Debug, Default)]
pub struct SystemMap {
    pub entries: Vec<SymEntry>,
}

impl SystemMap {
    /// Loads the System.map file.
    pub fn load(path: &str) -> Result<SystemMap, String> {
        let f = std::fs::File::open(path).map_err(|e| format!("{}: {}", path, e))?;
        let mut result = SystemMap::default();
        for line in std::io::BufReader::new(f).lines() {
            let line = line.map_err(|e| e.to_string())?;
            let parts: Vec<&str> = line.split(' ').collect();
            if parts.len() < 3 {
                continue;
            }
            let addr = match u64::from_str_radix(parts[0], 16) {
                Ok(a) => a,
                Err(_) => {
                    println!("invalid line: {}", line);
                    continue;
                }
            };
            let typ = parts[1].chars().next().unwrap_or('?');
            if let Some(last) = result.entries.last_mut() {
                last.end = addr;
            }
            result.entries.push(SymEntry {
                start: addr,
                end: addr,
                typ,
                name: parts[2].trim().to_string(),
            });
        }
        Ok(result)
    }

    /// Resolves the symbol containing addr. Returns the index of the
    /// entry.
    pub fn resolve_index(&self, addr: u64) -> Option<usize> {
        if self.entries.is_empty() {
            return None;
        }
        let idx = self.entries.partition_point(|e| e.start <= addr);
        if idx == 0 {
            return Some(0);
        }
        Some(idx - 1)
    }

    /// Resolves the symbol containing addr.
    pub fn resolve(&self, addr: u64) -> Option<&SymEntry> {
        self.resolve_index(addr).map(|i| &self.entries[i])
    }

    /// Looks up the symbol by name.
    pub fn lookup(&self, name: &str) -> Option<&SymEntry> {
        self.entries.iter().find(|e| e.name == name)
    }

    /// Maps a PC to kernel virtual address space. During early boot
    /// the kernel runs at its physical load address 0x80200000, which
    /// corresponds to virtual address 0xffffffff80000000.
    pub fn kernel_map(pc: u64) -> Option<u64> {
        // OpenSBI range: no kernel symbols here.
        if (0x8000_0000..0x8020_0000).contains(&pc) {
            return None;
        }
        if pc >= 0x8020_0000 && pc < 0x1_0000_0000 {
            return Some(pc - 0x20_0000 + 0xffff_ffff_0000_0000);
        }
        Some(pc)
    }

    /// Returns the function containing pc and the mapped address.
    pub fn func_name(&self, pc: u64) -> Option<(&SymEntry, u64)> {
        let mapped = Self::kernel_map(pc)?;
        self.resolve(mapped).map(|e| (e, mapped))
    }
}
