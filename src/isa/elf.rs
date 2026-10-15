//! Minimal ELF64 little-endian parser and the ELF disassembler.

use super::{decode, decode_c};
use std::fmt::Write as _;

pub const PT_NULL: u32 = 0;
pub const PT_LOAD: u32 = 1;
pub const PT_DYNAMIC: u32 = 2;
pub const PT_INTERP: u32 = 3;
pub const PT_NOTE: u32 = 4;
pub const PT_PHDR: u32 = 6;
pub const PT_TLS: u32 = 7;
pub const PT_GNU_STACK: u32 = 0x6474e551;

pub const PF_X: u32 = 1;
pub const PF_W: u32 = 2;
pub const PF_R: u32 = 4;

pub const ET_EXEC: u16 = 2;
pub const ET_DYN: u16 = 3;

pub const EM_RISCV: u16 = 243;

pub const STT_FUNC: u8 = 2;

/// ELF program header.
#[derive(Clone, Debug, Default)]
pub struct ProgHeader {
    pub typ: u32,
    pub flags: u32,
    pub offset: u64,
    pub vaddr: u64,
    pub paddr: u64,
    pub filesz: u64,
    pub memsz: u64,
    pub align: u64,
}

/// ELF section header.
#[derive(Clone, Debug, Default)]
pub struct Section {
    pub name: String,
    pub typ: u32,
    pub flags: u64,
    pub addr: u64,
    pub offset: u64,
    pub size: u64,
    pub link: u32,
    pub info: u32,
    pub entsize: u64,
}

/// ELF symbol.
#[derive(Clone, Debug, Default)]
pub struct Symbol {
    pub name: String,
    pub info: u8,
    pub shndx: u16,
    pub value: u64,
    pub size: u64,
}

impl Symbol {
    pub fn typ(&self) -> u8 {
        self.info & 0xf
    }
}

/// A parsed ELF64 file.
#[derive(Clone, Debug, Default)]
pub struct File {
    pub data: Vec<u8>,
    pub class: u8,
    pub endian: u8,
    pub version: u8,
    pub osabi: u8,
    pub abiversion: u8,
    pub typ: u16,
    pub machine: u16,
    pub entry: u64,
    pub phoff: u64,
    pub progs: Vec<ProgHeader>,
    pub sections: Vec<Section>,
}

fn u16_at(d: &[u8], o: usize) -> Result<u16, String> {
    d.get(o..o + 2)
        .map(|b| u16::from_le_bytes([b[0], b[1]]))
        .ok_or_else(|| "truncated ELF".to_string())
}

fn u32_at(d: &[u8], o: usize) -> Result<u32, String> {
    d.get(o..o + 4)
        .map(|b| u32::from_le_bytes(b.try_into().unwrap()))
        .ok_or_else(|| "truncated ELF".to_string())
}

fn u64_at(d: &[u8], o: usize) -> Result<u64, String> {
    d.get(o..o + 8)
        .map(|b| u64::from_le_bytes(b.try_into().unwrap()))
        .ok_or_else(|| "truncated ELF".to_string())
}

fn cstr(d: &[u8], o: usize) -> String {
    if o >= d.len() {
        return String::new();
    }
    let end = d[o..].iter().position(|&b| b == 0).unwrap_or(d.len() - o);
    String::from_utf8_lossy(&d[o..o + end]).to_string()
}

/// Tests if data starts with the ELF magic.
pub fn is_elf(data: &[u8]) -> bool {
    data.len() >= 4 && data[..4] == [0x7f, b'E', b'L', b'F']
}

impl File {
    /// Opens and parses the ELF file.
    pub fn open(path: &str) -> Result<File, String> {
        let data = std::fs::read(path).map_err(|e| format!("{}: {}", path, e))?;
        File::parse(data)
    }

    /// Parses the ELF file data.
    pub fn parse(data: Vec<u8>) -> Result<File, String> {
        if !is_elf(&data) {
            return Err("not an ELF file".to_string());
        }
        if data.len() < 64 {
            return Err("truncated ELF header".to_string());
        }
        if data[4] != 2 {
            return Err("unsupported ELF class (only ELF64 supported)".to_string());
        }
        if data[5] != 1 {
            return Err("unsupported ELF byte order".to_string());
        }
        let mut f = File {
            class: data[4],
            endian: data[5],
            version: data[6],
            osabi: data[7],
            abiversion: data[8],
            typ: u16_at(&data, 16)?,
            machine: u16_at(&data, 18)?,
            entry: u64_at(&data, 24)?,
            phoff: u64_at(&data, 32)?,
            ..Default::default()
        };
        let shoff = u64_at(&data, 40)? as usize;
        let phentsize = u16_at(&data, 54)? as usize;
        let phnum = u16_at(&data, 56)? as usize;
        let shentsize = u16_at(&data, 58)? as usize;
        let shnum = u16_at(&data, 60)? as usize;
        let shstrndx = u16_at(&data, 62)? as usize;

        for i in 0..phnum {
            let o = f.phoff as usize + i * phentsize;
            f.progs.push(ProgHeader {
                typ: u32_at(&data, o)?,
                flags: u32_at(&data, o + 4)?,
                offset: u64_at(&data, o + 8)?,
                vaddr: u64_at(&data, o + 16)?,
                paddr: u64_at(&data, o + 24)?,
                filesz: u64_at(&data, o + 32)?,
                memsz: u64_at(&data, o + 40)?,
                align: u64_at(&data, o + 48)?,
            });
        }

        let mut name_offsets = Vec::new();
        for i in 0..shnum {
            let o = shoff + i * shentsize;
            name_offsets.push(u32_at(&data, o)? as usize);
            f.sections.push(Section {
                name: String::new(),
                typ: u32_at(&data, o + 4)?,
                flags: u64_at(&data, o + 8)?,
                addr: u64_at(&data, o + 16)?,
                offset: u64_at(&data, o + 24)?,
                size: u64_at(&data, o + 32)?,
                link: u32_at(&data, o + 40)?,
                info: u32_at(&data, o + 44)?,
                entsize: u64_at(&data, o + 56)?,
            });
        }
        if shstrndx < f.sections.len() {
            let stroff = f.sections[shstrndx].offset as usize;
            for (i, s) in f.sections.iter_mut().enumerate() {
                s.name = cstr(&data, stroff + name_offsets[i]);
            }
        }
        f.data = data;
        Ok(f)
    }

    /// Returns the file data of the program segment.
    pub fn prog_data(&self, prog: &ProgHeader) -> &[u8] {
        let start = (prog.offset as usize).min(self.data.len());
        let end = (prog.offset + prog.filesz).min(self.data.len() as u64) as usize;
        &self.data[start..end]
    }

    /// Returns the section data.
    pub fn section_data(&self, s: &Section) -> &[u8] {
        if s.typ == 8 {
            // SHT_NOBITS
            return &[];
        }
        let start = (s.offset as usize).min(self.data.len());
        let end = (s.offset + s.size).min(self.data.len() as u64) as usize;
        &self.data[start..end]
    }

    /// Returns the section by name.
    pub fn section(&self, name: &str) -> Option<&Section> {
        self.sections.iter().find(|s| s.name == name)
    }

    fn symbols_of_type(&self, typ: u32) -> Vec<Symbol> {
        let mut result = Vec::new();
        for s in &self.sections {
            if s.typ != typ {
                continue;
            }
            let data = self.section_data(s);
            let strtab = match self.sections.get(s.link as usize) {
                Some(st) => self.section_data(st),
                None => continue,
            };
            let mut o = 24; // Skip the null symbol.
            while o + 24 <= data.len() {
                let name = u32_at(data, o).unwrap_or(0) as usize;
                result.push(Symbol {
                    name: cstr(strtab, name),
                    info: data[o + 4],
                    shndx: u16_at(data, o + 6).unwrap_or(0),
                    value: u64_at(data, o + 8).unwrap_or(0),
                    size: u64_at(data, o + 16).unwrap_or(0),
                });
                o += 24;
            }
        }
        result
    }

    /// Returns the symbol table entries (SHT_SYMTAB).
    pub fn symbols(&self) -> Vec<Symbol> {
        self.symbols_of_type(2)
    }

    /// Returns the dynamic symbol table entries (SHT_DYNSYM).
    pub fn dynamic_symbols(&self) -> Vec<Symbol> {
        self.symbols_of_type(11)
    }

    /// Returns the program interpreter path, if any.
    pub fn interp(&self) -> Option<String> {
        self.progs
            .iter()
            .find(|p| p.typ == PT_INTERP)
            .map(|p| cstr(self.prog_data(p), 0))
    }
}

/// Formats data as a hex dump in the style of Go's hex.Dump.
pub fn hex_dump(data: &[u8]) -> String {
    let mut out = String::new();
    for (i, chunk) in data.chunks(16).enumerate() {
        let _ = write!(out, "{:08x}  ", i * 16);
        for j in 0..16 {
            if j < chunk.len() {
                let _ = write!(out, "{:02x} ", chunk[j]);
            } else {
                out.push_str("   ");
            }
            if j == 7 {
                out.push(' ');
            }
        }
        out.push_str(" |");
        for &b in chunk {
            out.push(if (0x20..0x7f).contains(&b) {
                b as char
            } else {
                '.'
            });
        }
        out.push_str("|\n");
    }
    out
}

fn prog_type_name(t: u32) -> String {
    match t {
        PT_NULL => "PT_NULL".into(),
        PT_LOAD => "PT_LOAD".into(),
        PT_DYNAMIC => "PT_DYNAMIC".into(),
        PT_INTERP => "PT_INTERP".into(),
        PT_NOTE => "PT_NOTE".into(),
        PT_PHDR => "PT_PHDR".into(),
        PT_TLS => "PT_TLS".into(),
        PT_GNU_STACK => "PT_GNU_STACK".into(),
        0x70000003 => "PT_RISCV_ATTRIBUTES".into(),
        _ => format!("0x{:x}", t),
    }
}

fn prog_flags_string(f: u32) -> String {
    let mut parts = Vec::new();
    if f & PF_X != 0 {
        parts.push("PF_X");
    }
    if f & PF_W != 0 {
        parts.push("PF_W");
    }
    if f & PF_R != 0 {
        parts.push("PF_R");
    }
    parts.join("+")
}

/// Disassembles a buffer of machine code starting at address pc.
pub fn disassemble(mut pc: u64, mut data: &[u8], out: &mut String) -> Result<(), String> {
    let mut last_desc = super::Op::Invalid;
    while !data.is_empty() {
        let (instr, size, raw) = if data[0] & 0b11 == 0b11 {
            if data.len() < 4 {
                return Err("truncated .text".to_string());
            }
            let raw = u32::from_le_bytes(data[..4].try_into().unwrap());
            (decode(raw)?, 4usize, raw)
        } else {
            if data.len() < 2 {
                return Err("truncated .text".to_string());
            }
            let raw = u16::from_le_bytes([data[0], data[1]]) as u32;
            (decode_c(raw as u16)?, 2usize, raw)
        };
        let mut line = if size == 4 {
            format!("{:8x}:  {:08x}   {}", pc, raw, instr)
        } else {
            format!("{:8x}:  {:04x}       {}", pc, raw, instr)
        };
        let desc = instr.op.desc();
        if !desc.is_empty() && instr.op != last_desc {
            last_desc = instr.op;
            while line.len() < 47 {
                line.push(' ');
            }
            let _ = write!(line, "# {}", desc);
        }
        out.push_str(&line);
        out.push('\n');
        data = &data[size..];
        pc += size as u64;
    }
    Ok(())
}

/// Prints the ELF file information and disassembles its .text section.
pub fn decode_elf(path: &str) -> Result<String, String> {
    let f = File::open(path)?;
    let mut out = String::new();

    let _ = writeln!(out, "File:");
    let _ = writeln!(
        out,
        " - Class     : {}",
        if f.class == 2 { "ELFCLASS64" } else { "?" }
    );
    let _ = writeln!(
        out,
        " - Data      : {}",
        if f.endian == 1 { "ELFDATA2LSB" } else { "?" }
    );
    let _ = writeln!(out, " - Version   : {}", f.version);
    let _ = writeln!(out, " - OSABI     : {}", f.osabi);
    let _ = writeln!(out, " - ABIVersion: {}", f.abiversion);
    let _ = writeln!(out, " - ByteOrder : LittleEndian");
    let _ = writeln!(
        out,
        " - Type      : {}",
        match f.typ {
            ET_EXEC => "ET_EXEC".to_string(),
            ET_DYN => "ET_DYN".to_string(),
            t => format!("{}", t),
        }
    );
    let _ = writeln!(
        out,
        " - Machine   : {}",
        if f.machine == EM_RISCV {
            "EM_RISCV".to_string()
        } else {
            format!("{}", f.machine)
        }
    );
    let _ = writeln!(out, " - Entry     : {}", f.entry);

    for (idx, prog) in f.progs.iter().enumerate() {
        let _ = writeln!(out, "Prog {}", idx);
        let _ = writeln!(out, " - Type : {}", prog_type_name(prog.typ));
        let _ = writeln!(out, " - Flags: {}", prog_flags_string(prog.flags));
        let _ = writeln!(out, " - Vaddr: {:x}", prog.vaddr);
        let _ = writeln!(out, " - Memsz: {:x}", prog.memsz);
        let _ = writeln!(out, " - Align: {:x}", prog.align);
        if prog.typ == PT_LOAD {
            let data = f.prog_data(prog);
            let limit = 256;
            if data.len() > limit {
                out.push_str(&hex_dump(&data[..limit]));
                let _ = writeln!(out, "...{} bytes omitted...", data.len() - limit);
            } else {
                out.push_str(&hex_dump(data));
            }
        }
    }

    let mut text_idx = None;
    for (idx, s) in f.sections.iter().enumerate() {
        let _ = writeln!(out, "Section  {}", s.name);
        let _ = writeln!(out, " - Type: {}", s.typ);
        let _ = writeln!(out, " - Addr: {:x}", s.addr);
        let _ = writeln!(out, " - Size: {:x}", s.size);
        match s.name.as_str() {
            ".text" => text_idx = Some(idx),
            ".rodata" => {
                let data = f.section_data(s);
                out.push_str(&hex_dump(&data[..data.len().min(32)]));
            }
            _ => {}
        }
    }
    let text_idx = text_idx.ok_or_else(|| ".text section not found".to_string())?;
    let text = &f.sections[text_idx];
    let _ = writeln!(out, ".text.Type={}", text.typ);

    let mut symbols = f.symbols();
    if symbols.is_empty() {
        symbols = f.dynamic_symbols();
    }
    for sym in &symbols {
        if sym.typ() != STT_FUNC || sym.shndx as usize != text_idx {
            continue;
        }
        let _ = writeln!(out, "{:016x} <{}>:", sym.value, sym.name);
    }
    let _ = writeln!(out, "{:016x} <_start>:", f.entry);

    let data = f.section_data(text);
    let _ = writeln!(
        out,
        ".text at 0x{:x}, size {} bytes",
        text.addr,
        data.len()
    );
    disassemble(text.addr, data, &mut out)?;
    Ok(out)
}
