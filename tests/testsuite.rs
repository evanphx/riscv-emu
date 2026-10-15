//! Runs the riscv-tests ISA suite and the vector test suite from
//! testdata/. Each test runs in M-mode with physical addressing and
//! reports its result through the HTIF tohost register.

use riscv::cpu::Cpu;
use riscv::dev::Htif;
use riscv::isa::{self, elf, Mode};
use riscv::memory::{Memory, RAM_BASE};
use std::path::Path;
use std::sync::atomic::Ordering;
use std::sync::Arc;

/// Tests that are not run: hypervisor tests, and extensions that are
/// not implemented (Zfh, Svnapot, Svade dirty bit faults).
const SKIP: &[&str] = &[
    "Makefile",
    "hypervisor-p-2-stage_translation",
    "hypervisor-p-2-stage_translation_implicit_load_error",
    "hypervisor-p-2-stage_translation_implicit_load_error_hs",
    "hypervisor-svadu-p-2-stage_translation_implicit_store_error",
    "hypervisor-svadu-p-2-stage_translation_implicit_store_error_hs",
    "rv64si-p-dirty",
    // The test binary is built position independent and its GOT
    // follows the 16-byte test data immediately. Zeroing the 64-byte
    // cache block also wipes the GOT, so the test cannot pass with a
    // 64-byte Zicboz block size.
    "rv64mzicbo-p-zero",
    "rv64ssvnapot-p-napot",
    "rv64uzfh-p-fadd",
    "rv64uzfh-p-fclass",
    "rv64uzfh-p-fcmp",
    "rv64uzfh-p-fcvt",
    "rv64uzfh-p-fcvt_w",
    "rv64uzfh-p-fdiv",
    "rv64uzfh-p-fmadd",
    "rv64uzfh-p-fmin",
    "rv64uzfh-p-ldst",
    "rv64uzfh-p-move",
    "rv64uzfh-p-recoding",
];

/// Maximum number of instructions a test may execute.
const MAX_INSTRUCTIONS: u64 = 50_000_000;

/// Runs the test ELF file. Returns Ok(None) on success, and
/// Ok(Some(n)) if test case n failed.
fn run_test(file: &Path, jit: bool) -> Result<Option<u64>, String> {
    let mem = Arc::new(Memory::new(RAM_BASE, 0x200_0000));
    let mut cpu = Cpu::new(mem.clone());
    cpu.set_mode(Mode::M);

    let f = elf::File::open(file.to_str().unwrap())?;
    for prog in &f.progs {
        if prog.typ != elf::PT_LOAD {
            continue;
        }
        if !mem.contains(prog.paddr) || !mem.contains(prog.paddr + prog.memsz - 1) {
            return Err(format!(
                "prog out of range: {:x}...{:x}",
                prog.paddr,
                prog.paddr + prog.memsz
            ));
        }
        mem.write_bytes(mem.offset(prog.paddr), f.prog_data(prog));
    }

    let mut to = (0, 0);
    let mut from = (0, 0);
    for sym in f.symbols() {
        match sym.name.as_str() {
            "tohost" => to = (sym.value, sym.size),
            "fromhost" => from = (sym.value, sym.size),
            _ => {}
        }
    }
    if to.0 == 0 {
        return Err("tohost undefined".to_string());
    }
    let (start, size) = if to.0 < from.0 {
        (to.0, from.0 + from.1 - to.0)
    } else {
        (from.0, to.0 + to.1 - from.0)
    };
    let htif = Htif::new(cpu.signals.clone(), start, size, to.0, from.0);
    let status = htif.exit_status.clone();
    cpu.overlay = Some(Box::new(htif));
    cpu.pc = f.entry;

    if jit {
        enable_jit(&mut cpu)?;
        cpu.instret_limit = MAX_INSTRUCTIONS;
        cpu.run()?;
    } else {
        while !cpu.signals.is_shutdown() {
            if cpu.instret > MAX_INSTRUCTIONS {
                return Err("timeout".to_string());
            }
            cpu.step()?;
        }
    }

    let status = status.load(Ordering::SeqCst);
    if status != 1 {
        return Ok(Some(status >> 1));
    }
    Ok(None)
}

/// Enables the JIT so that every block is compiled on first use.
#[cfg(feature = "jit")]
fn enable_jit(cpu: &mut Cpu) -> Result<(), String> {
    cpu.enable_jit(1, false)
}

#[cfg(not(feature = "jit"))]
fn enable_jit(_cpu: &mut Cpu) -> Result<(), String> {
    Ok(())
}

fn test_suite(dir: &str, jit: bool) -> (usize, Vec<String>) {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join(dir);
    let mut entries: Vec<_> = std::fs::read_dir(&dir)
        .unwrap_or_else(|e| panic!("read_dir {:?}: {}", dir, e))
        .map(|e| e.unwrap().path())
        .collect();
    entries.sort();

    let mut count = 0;
    let mut failures = Vec::new();
    for path in entries {
        let name = path.file_name().unwrap().to_str().unwrap().to_string();
        if name.ends_with(".dump") || path.is_dir() || SKIP.contains(&name.as_str()) {
            continue;
        }
        count += 1;
        if std::env::var("RSEMU_TEST_VERBOSE").is_ok() {
            eprintln!("{}", name);
        }
        match run_test(&path, jit) {
            Ok(None) => {}
            Ok(Some(n)) => failures.push(format!("{}: assertion {}", name, n)),
            Err(e) => failures.push(format!("{}: {}", name, e)),
        }
    }
    (count, failures)
}

fn report(dir: &str, jit: bool) {
    isa::init_decode_tables();
    let (count, failures) = test_suite(dir, jit);
    for f in &failures {
        eprintln!("FAIL {}", f);
    }
    eprintln!(
        "{}{}: {} tests, {} success, {} fail",
        dir,
        if jit { " (JIT)" } else { "" },
        count,
        count - failures.len(),
        failures.len()
    );
    assert!(failures.is_empty(), "{} tests failed", failures.len());
}

#[test]
fn isa_tests() {
    report("testdata/isa", false);
}

#[test]
fn vpu_tests() {
    report("testdata/vpu", false);
}

#[cfg(feature = "jit")]
#[test]
fn isa_tests_jit() {
    report("testdata/isa", true);
}

#[cfg(feature = "jit")]
#[test]
fn vpu_tests_jit() {
    report("testdata/vpu", true);
}
