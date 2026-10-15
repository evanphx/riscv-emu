//! Runs the riscv-tests ISA suite and the vector test suite from
//! testdata/. Each test runs in M-mode with physical addressing and
//! reports its result through the HTIF tohost register.

use riscv::cpu::Cpu;
use riscv::dev::Htif;
use riscv::isa::{self, elf, Mode};
use riscv::memory::{Memory, RAM_BASE};
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
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

/// Number of JIT code cache blocks that ran.
static CACHE_HITS: AtomicU64 = AtomicU64::new(0);

/// Maximum number of instructions a test may execute.
const MAX_INSTRUCTIONS: u64 = 50_000_000;

/// How tests are executed.
#[derive(Clone, Copy)]
enum Exec<'a> {
    Interp,
    /// JIT compile every block on first use.
    Jit,
    /// JIT compile every block on first use, with optimizations.
    #[allow(dead_code)]
    JitOpt,
    /// JIT with the code cache file.
    #[allow(dead_code)]
    JitCache(&'a Path),
}

/// Runs the test ELF file. Returns Ok(None) on success, and
/// Ok(Some(n)) if test case n failed.
fn run_test(file: &Path, exec: Exec) -> Result<Option<u64>, String> {
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

    if let Exec::Jit | Exec::JitOpt | Exec::JitCache(_) = exec {
        enable_jit(&mut cpu, exec)?;
        cpu.instret_limit = MAX_INSTRUCTIONS;
        cpu.run()?;
        #[cfg(feature = "jit")]
        if let Some(stats) = cpu.jit_stats() {
            CACHE_HITS.fetch_add(stats.cache_hits, Ordering::Relaxed);
        }
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
fn enable_jit(cpu: &mut Cpu, exec: Exec) -> Result<(), String> {
    cpu.enable_jit(1, false)?;
    match exec {
        Exec::JitCache(path) => cpu.set_jit_cache(path),
        Exec::JitOpt => cpu.set_jit_optimize(true),
        _ => {}
    }
    Ok(())
}

#[cfg(not(feature = "jit"))]
fn enable_jit(_cpu: &mut Cpu, _exec: Exec) -> Result<(), String> {
    Ok(())
}

fn test_suite(dir: &str, exec: Exec, filter: impl Fn(&str) -> bool) -> (usize, Vec<String>) {
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
        if name.ends_with(".dump")
            || path.is_dir()
            || SKIP.contains(&name.as_str())
            || !filter(&name)
        {
            continue;
        }
        count += 1;
        if std::env::var("RSEMU_TEST_VERBOSE").is_ok() {
            eprintln!("{}", name);
        }
        match run_test(&path, exec) {
            Ok(None) => {}
            Ok(Some(n)) => failures.push(format!("{}: assertion {}", name, n)),
            Err(e) => failures.push(format!("{}: {}", name, e)),
        }
    }
    (count, failures)
}

fn report(dir: &str, exec: Exec, filter: impl Fn(&str) -> bool) {
    isa::init_decode_tables();
    let (count, failures) = test_suite(dir, exec, filter);
    for f in &failures {
        eprintln!("FAIL {}", f);
    }
    eprintln!(
        "{}{}: {} tests, {} success, {} fail",
        dir,
        match exec {
            Exec::Interp => "",
            Exec::Jit => " (JIT)",
            Exec::JitOpt => " (optimizing JIT)",
            Exec::JitCache(_) => " (JIT cache)",
        },
        count,
        count - failures.len(),
        failures.len()
    );
    assert!(failures.is_empty(), "{} tests failed", failures.len());
}

fn all(_: &str) -> bool {
    true
}

#[test]
fn isa_tests() {
    report("testdata/isa", Exec::Interp, all);
}

#[test]
fn vpu_tests() {
    report("testdata/vpu", Exec::Interp, all);
}

#[cfg(feature = "jit")]
#[test]
fn isa_tests_jit() {
    report("testdata/isa", Exec::Jit, all);
}

#[cfg(feature = "jit")]
#[test]
fn vpu_tests_jit() {
    report("testdata/vpu", Exec::Jit, all);
}

#[cfg(feature = "jit")]
#[test]
fn isa_tests_jit_opt() {
    report("testdata/isa", Exec::JitOpt, all);
}

/// Runs tests twice with a shared JIT code cache. All tests load their
/// code at the same addresses, so the cached blocks of one test must
/// be checked against the code of the others before they run.
#[cfg(feature = "jit")]
#[test]
fn isa_tests_jit_cache() {
    let dir = std::env::temp_dir().join(format!("rsemu-jit-cache-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("isa.jit");
    let filter = |name: &str| name.starts_with("rv64ui-p-") || name.starts_with("rv64ud-p-");
    report("testdata/isa", Exec::JitCache(&path), filter);
    let hits = CACHE_HITS.swap(0, Ordering::Relaxed);
    report("testdata/isa", Exec::JitCache(&path), filter);
    let hits2 = CACHE_HITS.load(Ordering::Relaxed);
    // In the second pass each test finds its own blocks in the cache.
    assert!(hits2 > hits, "cache hits: {} then {}", hits, hits2);
    std::fs::remove_dir_all(&dir).unwrap();
}
