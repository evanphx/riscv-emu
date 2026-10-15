//! Instruction tracing and debugging helpers.

use super::Cpu;
use crate::isa::{self, Instr, Mode, Op};

/// Saved register state for register change tracing.
pub(super) struct RegTrace {
    x: [u64; 32],
    f: [u64; 32],
    v: Vec<u8>,
}

const COMMENT_COLUMN: usize = 48;
const TRACE_PREFIX: &str = "                                                 # ";

/// Formats an address for traces.
pub fn fmt_addr(addr: u64) -> String {
    if addr < 0x1_0000_0000 {
        format!("  {:08x}", addr)
    } else if addr >= 0xffff_ffff_0000_0000 {
        format!("{:16x}", addr)
    } else {
        format!("{:10x}", addr)
    }
}

impl Cpu {
    pub fn color_on(&self) -> &'static str {
        if !self.color {
            return "";
        }
        match self.mode {
            Mode::S => "\x1b[30;106m",
            Mode::M => "\x1b[30;103m",
            _ => "\x1b[30m",
        }
    }

    pub fn color_off(&self) -> &'static str {
        if !self.color {
            return "";
        }
        "\x1b[0m"
    }

    /// Prints the function name of the PC if a symbol table is loaded.
    pub(super) fn trace_func(&self, pc: u64) {
        if let Some(sym) = &self.symtab {
            if let Some((e, mapped)) = sym.func_name(pc) {
                eprintln!(
                    "{}{:8x}  <{}+0x{:x}>:{}",
                    self.color_on(),
                    pc,
                    e.name,
                    mapped - e.start,
                    self.color_off()
                );
            }
        }
    }

    pub(super) fn tracef(&self, raw: u32, instr: &Instr, msg: &str) {
        self.pctrace(self.pc, raw, instr, msg);
    }

    pub(super) fn pctrace(&self, pc: u64, raw: u32, instr: &Instr, msg: &str) {
        let addr = fmt_addr(pc);
        let mut line = if raw & 0b11 == 0b11 {
            format!("{}:  {:08x}   {}", addr, raw, instr)
        } else {
            format!("{}:  {:04x}       {}", addr, raw, instr)
        };
        let mut msg = msg.to_string();
        if msg.is_empty() {
            match instr.op {
                Op::Auipc => msg = format!("pc={:x}, imm={:x}", self.pc, instr.imm),
                Op::Addi => {
                    msg = format!(
                        "{}={:x}, imm={:x}",
                        isa::reg_name(instr.rs1),
                        self.x[instr.rs1 as usize],
                        instr.imm
                    )
                }
                Op::Jal => msg = format!("imm={:x}", instr.imm),
                _ => {
                    if instr.rs1 != 0 {
                        msg = format!(
                            "{}={:x}",
                            isa::reg_name(instr.rs1),
                            self.x[instr.rs1 as usize]
                        );
                    }
                    if instr.rs2 != 0 {
                        if !msg.is_empty() {
                            msg.push(',');
                        }
                        msg.push_str(&format!(
                            "{}={:x}",
                            isa::reg_name(instr.rs2),
                            self.x[instr.rs2 as usize]
                        ));
                    }
                }
            }
        }
        if !msg.is_empty() {
            while line.len() < COMMENT_COLUMN {
                line.push(' ');
            }
            line.push_str(&format!(" # {}", msg));
        }
        eprintln!("{}{}{}", self.color_on(), line, self.color_off());
    }

    /// Traces the instruction and register changes (debug trace).
    pub(super) fn trace_instr(&mut self, raw: u32, instr: &Instr) {
        if let Some(sym) = &self.symtab {
            if let Some(mapped) = crate::cpu::SystemMap::kernel_map(self.pc) {
                if let Some(idx) = sym.resolve_index(mapped) {
                    if Some(idx) != self.last_symbol {
                        let e = &sym.entries[idx];
                        eprintln!(
                            "{}  <{}+0x{:x}>:\r",
                            fmt_addr(self.pc),
                            e.name,
                            mapped - e.start
                        );
                        self.last_symbol = Some(idx);
                    }
                }
            }
        }
        self.trace_regs();
        self.tracef(raw, instr, "");
    }

    fn trace_regs(&mut self) {
        let Some(rt) = self.reg_trace.as_mut() else {
            self.reg_trace = Some(Box::new(RegTrace {
                x: self.x,
                f: self.f,
                v: self.vpu.vregs.clone(),
            }));
            return;
        };
        for i in 0..32 {
            if self.x[i] != rt.x[i] {
                eprintln!("{}{}={:016x}", TRACE_PREFIX, isa::reg_name(i as u8), self.x[i]);
                rt.x[i] = self.x[i];
            }
        }
        for i in 0..32 {
            if self.f[i] != rt.f[i] {
                eprintln!("{}F{}={:016x}", TRACE_PREFIX, i, self.f[i]);
                rt.f[i] = self.f[i];
            }
        }
        let vlenb = (self.vpu.vlen / 8) as usize;
        for i in 0..32 {
            let r = &self.vpu.vregs[i * vlenb..(i + 1) * vlenb];
            if r != &rt.v[i * vlenb..(i + 1) * vlenb] {
                let hex: String = r.iter().map(|b| format!("{:02x}", b)).collect();
                eprintln!("{}v{}={}", TRACE_PREFIX, i, hex);
                rt.v[i * vlenb..(i + 1) * vlenb].copy_from_slice(r);
            }
        }
    }

    /// Disassembles the kernel function containing vaddr.
    pub fn disassemble_kernel(&self, vaddr: u64) {
        let Some(sym) = &self.symtab else {
            return;
        };
        let Some((entry, _)) = sym.func_name(vaddr) else {
            eprintln!("vaddr {:x} not in kernel map\r", vaddr);
            return;
        };
        let to_phys = |a: u64| -> u64 {
            if a < 0x1_0000_0000 {
                a
            } else {
                a.wrapping_sub(0xffff_ffff_0000_0000).wrapping_add(0x20_0000)
            }
        };
        let start = to_phys(entry.start);
        let end = to_phys(entry.end);
        let mem = &self.mem;
        if !mem.contains(start) || !mem.contains(end) {
            eprintln!("entry {:x}-{:x} outside of RAM range\r", start, end);
            return;
        }
        eprintln!("Disassembly of exception function:\r");
        eprintln!("{} <{}>:\r", fmt_addr(entry.start), entry.name);
        let mut a = start;
        while a < end {
            let lo = mem.read_u16(mem.offset(a)) as u32;
            let (instr, size, raw) = if lo & 0b11 == 0b11 {
                let raw = mem.read_u32(mem.offset(a));
                (isa::decode(raw), 4, raw)
            } else {
                (isa::decode_c(lo as u16), 2, lo)
            };
            let instr = match instr {
                Ok(i) => i,
                Err(e) => {
                    eprintln!("disassemble_kernel: {}\r", e);
                    return;
                }
            };
            let pc = entry.start + a - start;
            let mut line = if size == 4 {
                format!("{}:  {:08x}   {}", fmt_addr(pc), raw, instr)
            } else {
                format!("{}:  {:04x}       {}", fmt_addr(pc), raw, instr)
            };
            if pc == vaddr {
                line.push_str("\t# offending instruction");
            }
            eprintln!("{}\r", line);
            a += size;
        }
        eprintln!("End of exception function disassembly\r");
    }
}
