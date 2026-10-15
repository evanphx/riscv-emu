//! riscv-objdump: prints ELF file information and disassembles the
//! .text section.

fn main() {
    let files: Vec<String> = std::env::args().skip(1).collect();
    if files.is_empty() {
        eprintln!("usage: riscv-objdump file...");
        std::process::exit(2);
    }
    for file in &files {
        match riscv::isa::elf::decode_elf(file) {
            Ok(out) => print!("{}", out),
            Err(e) => {
                eprintln!("failed to disassemble {}: {}", file, e);
                std::process::exit(1);
            }
        }
    }
}
