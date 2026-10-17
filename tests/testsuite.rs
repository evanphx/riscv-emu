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
    Direct(bool),
    DirectMmap(bool),
    /// JIT with the code cache file.
    #[allow(dead_code)]
    JitCache(&'a Path),
}

/// Runs the test ELF file. Returns Ok(None) on success, and
/// Ok(Some(n)) if test case n failed.
fn run_test(file: &Path, exec: Exec) -> Result<Option<u64>, String> {
    let mem = Arc::new(if matches!(exec, Exec::DirectMmap(_)) {
        Memory::new_shared(RAM_BASE, 0x200_0000).unwrap()
    } else {
        Memory::new(RAM_BASE, 0x200_0000)
    });
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

    if let Exec::Jit | Exec::JitOpt | Exec::Direct(_) | Exec::JitCache(_) = exec {
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
    cpu.enable_jit(1, 0)?;
    match exec {
        Exec::Direct(opt) => {
            cpu.set_jit_backend("direct")?;
            cpu.set_jit_optimize(opt);
        }
        Exec::DirectMmap(opt) => {
            cpu.set_jit_backend("direct-mmap")?;
            cpu.set_jit_optimize(opt);
        }
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
            Exec::Direct(false) => " (direct JIT)",
            Exec::Direct(true) => " (direct regions)",
            Exec::DirectMmap(false) => " (direct mmap)",
            Exec::DirectMmap(true) => " (direct mmap regions)",
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

#[cfg(feature = "jit")]
#[test]
fn isa_tests_direct() {
    report("testdata/isa", Exec::Direct(false), all);
    report("testdata/isa", Exec::Direct(true), all);
}

#[cfg(feature = "jit")]
#[test]
fn vpu_tests_direct() {
    report("testdata/vpu", Exec::Direct(false), all);
}

#[cfg(feature = "jit")]
#[test]
fn isa_tests_direct_mmap() {
    report("testdata/isa", Exec::DirectMmap(false), all);
    report("testdata/isa", Exec::DirectMmap(true), all);
}

#[cfg(feature = "jit")]
#[test]
fn vpu_tests_direct_mmap() {
    report("testdata/vpu", Exec::DirectMmap(false), all);
}

#[cfg(feature = "jit")]
fn backend_ram(backend: &str, size: u64) -> Arc<Memory> {
    Arc::new(if backend == "direct-mmap" {
        Memory::new_shared(RAM_BASE, size).unwrap()
    } else {
        Memory::new(RAM_BASE, size)
    })
}

/// Unlike the HTIF suite, this has no overlay: stores can use the
/// direct RAM path. Verify dirty registers and exact counters at a
/// memory fault, and exercise the backend ABI boundary on cache reload.
#[cfg(feature = "jit")]
#[test]
fn direct_ram_fault_and_cache() {
    let path = std::env::temp_dir().join(format!("rsemu-direct-{}.jit", std::process::id()));
    let _ = std::fs::remove_file(&path);
    // sb a0,0(s0); lb t1,0(s0); add t2,t2,t1; addi a0,a0,-1;
    // addi s0,s0,1; bnez a0,-20; rdinstret a1; rdtime a2;
    // ld t3,0(s1) [fault]; addi t4,zero,99 [must not run].
    let words: [u32; 10] = [
        0x00a40023, 0x00040303, 0x006383b3, 0xfff50513, 0x00140413, 0xfe0516e3, 0xc02025f3,
        0xc0102673, 0x0004be03, 0x06300e93,
    ];
    for (index, backend) in [
        "",
        "cranelift",
        "direct",
        "direct",
        "direct-mmap",
        "direct-mmap",
        "cranelift",
    ]
    .iter()
    .enumerate()
    {
        let mem = backend_ram(backend, 0x4000);
        let bytes: Vec<u8> = words.iter().flat_map(|v| v.to_le_bytes()).collect();
        mem.write_bytes(0, &bytes);
        let mut cpu = Cpu::new(mem.clone());
        cpu.pc = RAM_BASE;
        cpu.x[8] = RAM_BASE + 0x2000;
        cpu.x[9] = RAM_BASE + 0x4000;
        cpu.x[10] = 131;
        cpu.instret_limit = 10_000;
        cpu.trap_handler = Some(Box::new(|_, trap| {
            assert_eq!(trap.cause, isa::CAUSE_LOAD_PAGE_FAULT);
            assert_eq!(trap.pc, RAM_BASE + 32);
            assert_eq!(trap.tval, RAM_BASE + 0x4000);
            Err("expected fault".into())
        }));
        if !backend.is_empty() {
            cpu.enable_jit(1, 0).unwrap();
            cpu.set_jit_backend(backend).unwrap();
            cpu.set_jit_cache(&path);
        }
        assert_eq!(cpu.run().unwrap_err(), "expected fault");
        assert_eq!(cpu.x[0], 0);
        assert_eq!(cpu.x[7], 7622); // sum of signed bytes 1..131
        assert_eq!(cpu.x[8], RAM_BASE + 0x2000 + 131);
        assert_eq!(cpu.x[10], 0);
        assert_eq!((cpu.x[11], cpu.x[12]), (786, 788));
        assert_eq!((cpu.instret, cpu.time), (789, 789));
        assert_eq!(cpu.x[29], 0);
        let mut data = [0u8; 131];
        mem.read_bytes(0x2000, &mut data);
        assert_eq!(data.as_slice(), (1..=131u8).rev().collect::<Vec<_>>());
        if let Some(stats) = cpu.jit_stats() {
            if index == 3 || index == 5 {
                assert!(stats.cache_hits > 0);
            } else {
                assert_eq!(stats.loaded, 0, "cache must not cross backend ABIs");
            }
        }
    }
    std::fs::remove_file(path).unwrap();
}

#[cfg(feature = "jit")]
#[test]
fn direct_mmap_reads_follow_remaps_and_permissions() {
    use riscv::mmu::{self, Pte, Satp, PTE_A, PTE_R, PTE_U, PTE_V, PTE_X, SATP_MODE_SV39};
    let words = [0x00043283u32, 0x00128293, 0x0004b303]; // ld; addi; unmapped ld
    for backend in ["", "cranelift", "direct", "direct-mmap"] {
        for opt in [false, true] {
            let mem = backend_ram(backend, 0x10000);
            let root = mem.alloc_page().unwrap();
            let code = mem.alloc_page().unwrap();
            let first = mem.alloc_page().unwrap();
            let second = mem.alloc_page().unwrap();
            let satp = Satp::new(SATP_MODE_SV39, root);
            mmu::set_map_sv39(&mem, satp, 4, code, PTE_R | PTE_X).unwrap();
            mmu::set_map_sv39(&mem, satp, 8, first, PTE_R | PTE_U).unwrap();
            mem.write_bytes(
                mem.page_offset_of(code).unwrap(),
                &words
                    .iter()
                    .flat_map(|v| v.to_le_bytes())
                    .collect::<Vec<_>>(),
            );
            mem.write_u64(mem.page_offset_of(first).unwrap(), 17);
            mem.write_u64(mem.page_offset_of(second).unwrap(), 39);
            // Both virtual addresses share root/L1 indices zero. Retain
            // the leaf location so later phases can edit this guest PTE.
            let l1 = Pte(mem.read_u64(mem.page_offset_of(root).unwrap())).ppn();
            let l0 = Pte(mem.read_u64(mem.page_offset_of(l1).unwrap())).ppn();
            let leaf = mem.page_offset_of(l0).unwrap() + 8 * 8;
            let mut cpu = Cpu::new(mem.clone());
            cpu.set_mode(Mode::S);
            cpu.set_satp(satp);
            cpu.x[9] = 0x9000;
            cpu.instret_limit = 1000;
            if !backend.is_empty() {
                cpu.enable_jit(1, 0).unwrap();
                cpu.set_jit_backend(backend).unwrap();
                cpu.set_jit_optimize(opt);
            }
            for phase in 0..9 {
                let sum = phase != 3 && phase != 6;
                let mxr = phase == 4;
                let mut status = cpu.mstatus();
                status.set_sum(sum);
                status.set_mxr(mxr);
                cpu.set_mstatus(status);
                if phase == 1 {
                    // Write through canonical RAM while the read alias is live.
                    mem.write_u64(mem.page_offset_of(first).unwrap(), 83);
                }
                if matches!(phase, 2 | 4 | 6) {
                    let flags = match phase {
                        4 => PTE_X | PTE_U,
                        6 => PTE_R,
                        _ => PTE_R | PTE_U,
                    };
                    mem.write_u64(leaf, Pte::make(second, flags | PTE_V | PTE_A).0);
                    cpu.flush_tlb_page(0x8000);
                }
                if phase == 8 {
                    mem.write_u64(leaf, Pte::make(first, PTE_R | PTE_U | PTE_V | PTE_A).0);
                    cpu.set_satp(satp);
                }
                let denied = matches!(phase, 3 | 5 | 7);
                cpu.pc = 0x4000;
                cpu.x[8] = if phase == 7 { 1 << 48 } else { 0x8000 };
                cpu.x[5] = 911;
                let address = if denied { cpu.x[8] } else { 0x9000 };
                cpu.trap_handler = Some(Box::new(move |_, trap| {
                    assert_eq!(trap.cause, isa::CAUSE_LOAD_PAGE_FAULT);
                    assert_eq!(trap.pc, if denied { 0x4000 } else { 0x4008 });
                    assert_eq!(trap.tval, address);
                    Err("expected fault".into())
                }));
                let before = cpu.instret;
                assert_eq!(cpu.run().unwrap_err(), "expected fault");
                let expected = match phase {
                    0 => 18,
                    1 | 8 => 84,
                    3 | 5 | 7 => 911,
                    _ => 40,
                };
                assert_eq!(cpu.x[5], expected, "{backend}, phase {phase}");
                assert_eq!(cpu.instret - before, if denied { 1 } else { 3 });
            }
            if backend == "direct-mmap" {
                assert!(cpu.jit_stats().unwrap().mmap_faults > 0);
            }
        }
    }
}

#[cfg(feature = "jit")]
#[test]
fn direct_mmap_proven_alignment_preserves_loads_and_stores() {
    // Mask an unknown base, add a shifted index, then use both aligned
    // addresses and an unaligned base with a compensating negative offset.
    let words: [u32; 10] = [
        0xff847413, 0x00359593, 0x00b40433, 0x01840413, 0x00a43023, 0x00043283, 0xfff40413,
        0xfea42ea3, 0xffd42303, 0x0004b383,
    ];
    for backend in ["", "direct", "direct-mmap"] {
        for opt in [false, true] {
            for (low, index) in [(1, 3), (7, 2), (0, 5)] {
                let mem = backend_ram(backend, 0x4000);
                mem.write_bytes(
                    0,
                    &words
                        .iter()
                        .flat_map(|v| v.to_le_bytes())
                        .collect::<Vec<_>>(),
                );
                let mut cpu = Cpu::new(mem.clone());
                cpu.pc = RAM_BASE;
                cpu.x[8] = RAM_BASE + 0x2010 + low;
                cpu.x[9] = RAM_BASE + 0x4000;
                cpu.x[10] = 0x1234_5678_89ab_cdef;
                cpu.x[11] = index;
                cpu.x[7] = 107;
                cpu.trap_handler = Some(Box::new(|_, trap| {
                    assert_eq!(trap.cause, isa::CAUSE_LOAD_PAGE_FAULT);
                    assert_eq!(trap.pc, RAM_BASE + 36);
                    assert_eq!(trap.tval, RAM_BASE + 0x4000);
                    Err("expected fault".into())
                }));
                if !backend.is_empty() {
                    cpu.enable_jit(1, 0).unwrap();
                    cpu.set_jit_backend(backend).unwrap();
                    cpu.set_jit_optimize(opt);
                }
                assert_eq!(cpu.run().unwrap_err(), "expected fault");
                let address = 0x2010 + 8 * index + 24;
                assert_eq!(mem.read_u64(address), 0x1234_5678_89ab_cdef);
                assert_eq!(mem.read_u32(address - 4), 0x89ab_cdef);
                assert_eq!(cpu.x[5], 0x1234_5678_89ab_cdef);
                assert_eq!(cpu.x[6], 0xffff_ffff_89ab_cdef);
                assert_eq!(cpu.x[7], 107);
                assert_eq!(cpu.x[8], RAM_BASE + address - 1);
                assert_eq!(cpu.instret, 10);
                if backend == "direct-mmap" {
                    assert!(cpu.jit_stats().unwrap().elided_alignment_guards >= 4);
                }
            }
        }
    }
}

#[cfg(feature = "jit")]
#[test]
fn direct_mmap_alignment_proofs_do_not_cross_branch_entries() {
    use riscv::mmu::{self, Satp, PTE_R, PTE_U, PTE_X, SATP_MODE_SV39};
    let half = 1u64 << 38;
    for opt in [false, true] {
        let mem = backend_ram("direct-mmap", 0x10000);
        let root = mem.alloc_page().unwrap();
        let code = mem.alloc_page().unwrap();
        let user = mem.alloc_page().unwrap();
        let supervisor = mem.alloc_page().unwrap();
        let satp = Satp::new(SATP_MODE_SV39, root);
        mmu::set_map_sv39(&mem, satp, 4, code, PTE_R | PTE_X | PTE_U).unwrap();
        mmu::set_map_sv39(&mem, satp, (half >> 12) - 1, user, PTE_R | PTE_U).unwrap();
        mmu::set_map_sv39(&mem, satp, half.wrapping_neg() >> 12, supervisor, PTE_R).unwrap();
        // The taken edge skips the mask. Reusing its alignment proof at
        // the join could read across the host window into the next bank.
        let words = [0x00060463u32, 0xff847413, 0x00043283, 0x0004b303];
        mem.write_bytes(
            mem.page_offset_of(code).unwrap(),
            &words
                .iter()
                .flat_map(|v| v.to_le_bytes())
                .collect::<Vec<_>>(),
        );
        mem.write_u64(
            mem.page_offset_of(user).unwrap() + 4088,
            0x1234_5678_9abc_def0,
        );
        mem.write_u64(
            mem.page_offset_of(supervisor).unwrap(),
            0xfedc_ba98_7654_3210,
        );
        let mut cpu = Cpu::new(mem.clone());
        cpu.set_satp(satp);
        cpu.enable_jit(1, 0).unwrap();
        cpu.set_jit_backend("direct-mmap").unwrap();
        cpu.set_jit_optimize(opt);
        // Populate adjacent windows: U's highest page, then S's lowest.
        cpu.set_mode(Mode::U);
        cpu.load64(half - 8).unwrap();
        cpu.set_mode(Mode::S);
        cpu.load64(half.wrapping_neg()).unwrap();
        cpu.set_mode(Mode::U);
        cpu.pc = 0x4000;
        cpu.x[8] = half - 4;
        cpu.x[9] = 0x9000;
        cpu.x[5] = 911;
        cpu.trap_handler = Some(Box::new(move |_, trap| {
            assert_eq!(trap.cause, isa::CAUSE_LOAD_PAGE_FAULT);
            assert_eq!(trap.pc, 0x4008);
            assert_eq!(trap.tval, half);
            Err("expected boundary fault".into())
        }));
        assert_eq!(cpu.run().unwrap_err(), "expected boundary fault");
        assert_eq!(cpu.x[5], 911);
        assert_eq!(cpu.instret, 2);
    }
}

#[cfg(feature = "jit")]
#[test]
fn direct_mmap_stores_follow_permissions_and_dirty_bits() {
    use riscv::mmu::{
        self, Pte, Satp, PTE_A, PTE_D, PTE_R, PTE_U, PTE_V, PTE_W, PTE_X, SATP_MODE_SV39,
    };
    for backend in ["", "direct", "direct-mmap"] {
        for opt in [false, true] {
            let mem = backend_ram(backend, 0x10000);
            let root = mem.alloc_page().unwrap();
            let code = mem.alloc_page().unwrap();
            let data = mem.alloc_page().unwrap();
            let satp = Satp::new(SATP_MODE_SV39, root);
            mmu::set_map_sv39(&mem, satp, 4, code, PTE_R | PTE_X).unwrap();
            mmu::set_map_sv39(&mem, satp, 8, data, PTE_R | PTE_W | PTE_U).unwrap();
            let words = [0x00a43023u32, 0x18a4332f, 0x00043283, 0x0004b303]; // sd; sc.d; ld; fault
            mem.write_bytes(
                mem.page_offset_of(code).unwrap(),
                &words
                    .iter()
                    .flat_map(|v| v.to_le_bytes())
                    .collect::<Vec<_>>(),
            );
            let l1 = Pte(mem.read_u64(mem.page_offset_of(root).unwrap())).ppn();
            let l0 = Pte(mem.read_u64(mem.page_offset_of(l1).unwrap())).ppn();
            let leaf = mem.page_offset_of(l0).unwrap() + 8 * 8;
            let mut cpu = Cpu::new(mem.clone());
            cpu.set_mode(Mode::S);
            cpu.set_satp(satp);
            if !backend.is_empty() {
                cpu.enable_jit(1, 0).unwrap();
                cpu.set_jit_backend(backend).unwrap();
                cpu.set_jit_optimize(opt);
            }
            for phase in 0..7 {
                let mut status = cpu.mstatus();
                status.set_sum(phase != 3);
                cpu.set_mstatus(status);
                if phase == 1 || phase == 2 {
                    let flags = PTE_R | PTE_U | PTE_A | PTE_V | if phase == 2 { PTE_W } else { 0 };
                    mem.write_u64(leaf, Pte::make(data, flags).0);
                    cpu.flush_tlb_page(0x8000);
                }
                cpu.pc = 0x4000;
                cpu.x[8] = match phase {
                    4 => 1 << 48,
                    5 => 0x8ffc,
                    _ => 0x8000,
                };
                cpu.x[9] = 0x9000;
                cpu.x[10] = 83 + phase;
                cpu.reservation = 0x8000;
                cpu.reservation_valid = true;
                let denied = matches!(phase, 1 | 3 | 4 | 5);
                let address = if phase == 5 {
                    0x9003
                } else if denied {
                    cpu.x[8]
                } else {
                    0x9000
                };
                cpu.trap_handler = Some(Box::new(move |_, trap| {
                    assert_eq!(
                        trap.cause,
                        if denied {
                            isa::CAUSE_STORE_PAGE_FAULT
                        } else {
                            isa::CAUSE_LOAD_PAGE_FAULT
                        }
                    );
                    assert_eq!(trap.pc, if denied { 0x4000 } else { 0x400c });
                    assert_eq!(trap.tval, address);
                    Err("expected fault".into())
                }));
                let old = mem.read_u64(mem.page_offset_of(data).unwrap());
                let before = cpu.instret;
                assert_eq!(cpu.run().unwrap_err(), "expected fault");
                assert_eq!(cpu.instret - before, if denied { 1 } else { 4 });
                assert_eq!(
                    mem.read_u64(mem.page_offset_of(data).unwrap()),
                    if denied { old } else { 83 + phase }
                );
                if !denied {
                    assert_eq!(cpu.x[6], 1, "store must invalidate LR/SC reservation");
                    assert_eq!(cpu.x[5], 83 + phase);
                    assert_ne!(mem.read_u64(leaf) & PTE_D as u64, 0);
                }
                // A rejected cross-page store must not write its first bytes.
                assert_eq!(mem.read_u32(mem.page_offset_of(data).unwrap() + 4092), 0);
            }
        }
    }
}

#[cfg(feature = "jit")]
#[test]
fn direct_mmap_stores_invalidate_code_through_evicted_aliases() {
    use riscv::mmu::{self, Satp, PTE_R, PTE_W, PTE_X, SATP_MODE_SV39};
    for backend in ["direct", "direct-mmap"] {
        for opt in [false, true] {
            let mem = backend_ram(backend, 0x10000);
            let root = mem.alloc_page().unwrap();
            let writer = mem.alloc_page().unwrap();
            let target = mem.alloc_page().unwrap();
            let satp = Satp::new(SATP_MODE_SV39, root);
            mmu::set_map_sv39(&mem, satp, 4, writer, PTE_R | PTE_X).unwrap();
            for vpn in [8, 0x1008] {
                mmu::set_map_sv39(&mem, satp, vpn, target, PTE_R | PTE_W | PTE_X).unwrap();
            }
            let old = 0x0004b303_01100293u64; // addi x5,zero,17; unmapped ld
            let new = 0x0004b303_02700293u64; // addi x5,zero,39; unmapped ld
            mem.write_bytes(
                mem.page_offset_of(writer).unwrap(),
                &0x0004b303_00a43023u64.to_le_bytes(),
            ); // sd x10,0(x8); unmapped ld
            let mut cpu = Cpu::new(mem.clone());
            cpu.set_mode(Mode::S);
            cpu.set_satp(satp);
            cpu.enable_jit(1, 0).unwrap();
            cpu.set_jit_backend(backend).unwrap();
            cpu.set_jit_optimize(opt);
            cpu.store64(0x1008000, old).unwrap();
            cpu.store64(0x8000, old).unwrap(); // evicts the first alias's software tag
            cpu.x[8] = 0x1008000;
            cpu.x[9] = 0x9000;
            cpu.x[10] = new;
            cpu.trap_handler = Some(Box::new(|_, trap| {
                assert_eq!(trap.cause, isa::CAUSE_LOAD_PAGE_FAULT);
                assert_eq!(trap.tval, 0x9000);
                Err("expected fault".into())
            }));
            for (pc, expected) in [(0x8000, 17), (0x4000, 17), (0x8000, 39)] {
                cpu.pc = pc;
                assert_eq!(cpu.run().unwrap_err(), "expected fault");
                assert_eq!(cpu.x[5], expected, "{backend}, opt={opt}");
            }
            assert!(cpu.jit_stats().unwrap().compiled > 0);
        }
    }
}

#[cfg(feature = "jit")]
#[test]
fn direct_memory_fast_and_slow_paths() {
    // Aligned half/word/double stores and signed loads, followed by
    // unaligned accesses and a double crossing a page boundary. A
    // final fault must preserve all earlier values and exact counters.
    let words: [u32; 13] = [
        0x00a41023, 0x00b42223, 0x00c43423, 0x00041683, 0x00442703, 0x00843783, 0x00a410a3,
        0x00b422a3, 0x00c437a3, 0x00141803, 0x00542883, 0x00f43903, 0x0004b983,
    ];
    for backend in ["", "cranelift", "direct", "direct-mmap"] {
        for opt in [false, true] {
            let mem = backend_ram(backend, 0x4000);
            let bytes: Vec<u8> = words.iter().flat_map(|v| v.to_le_bytes()).collect();
            mem.write_bytes(0, &bytes);
            let mut cpu = Cpu::new(mem.clone());
            cpu.pc = RAM_BASE;
            cpu.x[8] = RAM_BASE + 0x2ff0;
            cpu.x[9] = RAM_BASE + 0x4000;
            cpu.x[10] = 0xfedc;
            cpu.x[11] = 0x89ab_cdef;
            cpu.x[12] = 0x1234_5678_9abc_def1;
            cpu.x[19] = 23;
            cpu.instret_limit = 1000;
            cpu.trap_handler = Some(Box::new(|_, trap| {
                assert_eq!(trap.pc, RAM_BASE + 48);
                assert_eq!(trap.tval, RAM_BASE + 0x4000);
                assert_eq!(trap.cause, isa::CAUSE_LOAD_PAGE_FAULT);
                Err("expected fault".into())
            }));
            if !backend.is_empty() {
                cpu.enable_jit(1, 0).unwrap();
                cpu.set_jit_backend(backend).unwrap();
                cpu.set_jit_optimize(opt);
            }
            assert_eq!(cpu.run().unwrap_err(), "expected fault");
            let expected = [
                0xffff_ffff_ffff_fedc,
                0xffff_ffff_89ab_cdef,
                0x1234_5678_9abc_def1,
            ];
            assert_eq!(&cpu.x[13..16], &expected);
            assert_eq!(&cpu.x[16..19], &expected);
            assert_eq!(cpu.x[19], 23);
            assert_eq!((cpu.instret, cpu.time), (13, 13));
            let mut actual = [0; 23];
            mem.read_bytes(0x2ff0, &mut actual);
            let mut expected = [0; 23];
            expected[0..2].copy_from_slice(&0xfedcu16.to_le_bytes());
            expected[4..8].copy_from_slice(&0x89ab_cdefu32.to_le_bytes());
            expected[8..16].copy_from_slice(&0x1234_5678_9abc_def1u64.to_le_bytes());
            expected[1..3].copy_from_slice(&0xfedcu16.to_le_bytes());
            expected[5..9].copy_from_slice(&0x89ab_cdefu32.to_le_bytes());
            expected[15..23].copy_from_slice(&0x1234_5678_9abc_def1u64.to_le_bytes());
            assert_eq!(actual, expected);
        }
    }
}

#[cfg(feature = "jit")]
#[test]
fn direct_grouped_reads_preserve_fault_order() {
    // Warm a two-read loop, then move its base to the RAM boundary.
    // A failed range guard must not report the second read's fault
    // before the first read and intervening arithmetic have executed.
    let words: [u32; 10] = [
        0x00043283, 0x00138393, 0x01043303, 0xfff50513, 0xfe0518e3, 0x00048413, 0x00043283,
        0x00138393, 0x01043303, 0x00093e03,
    ];
    for backend in ["", "cranelift", "direct", "direct-mmap"] {
        for opt in [false, true] {
            for crosses in [false, true] {
                let mem = backend_ram(backend, 0x3000);
                let bytes: Vec<u8> = words.iter().flat_map(|v| v.to_le_bytes()).collect();
                mem.write_bytes(0, &bytes);
                for (offset, value) in [(0x2000, 17u64), (0x2010, 39), (0x2fe0, 83), (0x2ff0, 107)]
                {
                    mem.write_bytes(offset, &value.to_le_bytes());
                }
                let mut cpu = Cpu::new(mem);
                cpu.pc = RAM_BASE;
                cpu.x[8] = RAM_BASE + 0x2000;
                cpu.x[9] = RAM_BASE + if crosses { 0x2ff0 } else { 0x2fe0 };
                cpu.x[18] = RAM_BASE + 0x3000;
                cpu.x[10] = 20;
                cpu.instret_limit = 1000;
                cpu.trap_handler = Some(Box::new(move |_, trap| {
                    assert_eq!(trap.pc, RAM_BASE + if crosses { 32 } else { 36 });
                    assert_eq!(trap.tval, RAM_BASE + 0x3000);
                    assert_eq!(trap.cause, isa::CAUSE_LOAD_PAGE_FAULT);
                    Err("expected fault".into())
                }));
                if !backend.is_empty() {
                    cpu.enable_jit(1, 0).unwrap();
                    cpu.set_jit_backend(backend).unwrap();
                    cpu.set_jit_optimize(opt);
                }
                assert_eq!(cpu.run().unwrap_err(), "expected fault");
                assert_eq!(cpu.x[5], if crosses { 107 } else { 83 });
                assert_eq!(cpu.x[6], if crosses { 39 } else { 107 });
                assert_eq!(cpu.x[7], 21);
                assert_eq!(cpu.x[10], 0);
                let count = if crosses { 104 } else { 105 };
                assert_eq!((cpu.instret, cpu.time), (count, count));
            }
        }
    }
}

#[cfg(feature = "jit")]
#[test]
fn direct_profiled_loads_follow_changed_addresses() {
    // Train one load on a page, then change its base after compilation.
    // Exercise another slot, a colliding slot, and an unmapped page.
    // A fence keeps this focused on per-read guards, not loop hoisting.
    let words: [u32; 10] = [
        0x00043283, 0x00530333, 0xfff50513, 0x0000000f, 0xfe0518e3, 0x00048413, 0x01000513,
        0xfff58593, 0xfe0590e3, 0x00093383,
    ];
    for backend in ["", "cranelift", "direct", "direct-mmap"] {
        for opt in [false, true] {
            for moved in [0x3000, 0x1002000, 0x1004000] {
                let mem = backend_ram(backend, 0x1004000);
                let bytes: Vec<u8> = words.iter().flat_map(|v| v.to_le_bytes()).collect();
                mem.write_bytes(0, &bytes);
                mem.write_bytes(0x2000, &17u64.to_le_bytes());
                if moved < 0x1004000 {
                    mem.write_bytes(moved, &39u64.to_le_bytes());
                }
                let missing = moved == 0x1004000;
                let mut cpu = Cpu::new(mem);
                cpu.pc = RAM_BASE;
                cpu.x[8] = RAM_BASE + 0x2000;
                cpu.x[9] = RAM_BASE + moved;
                cpu.x[18] = RAM_BASE + 0x1004000;
                cpu.x[10] = 32;
                cpu.x[11] = 2;
                cpu.instret_limit = 10000;
                cpu.trap_handler = Some(Box::new(move |_, trap| {
                    assert_eq!(trap.pc, RAM_BASE + if missing { 0 } else { 36 });
                    assert_eq!(trap.tval, RAM_BASE + 0x1004000);
                    assert_eq!(trap.cause, isa::CAUSE_LOAD_PAGE_FAULT);
                    Err("expected fault".into())
                }));
                if !backend.is_empty() {
                    cpu.enable_jit(16, 0).unwrap();
                    cpu.set_jit_backend(backend).unwrap();
                    cpu.set_jit_optimize(opt);
                }
                assert_eq!(cpu.run().unwrap_err(), "expected fault");
                assert_eq!(cpu.x[5], if missing { 17 } else { 39 });
                assert_eq!(cpu.x[6], if missing { 544 } else { 1168 });
                assert_eq!(cpu.x[10], 16);
                assert_eq!(cpu.x[11], if missing { 1 } else { 0 });
                let count = if missing { 165 } else { 249 };
                assert_eq!((cpu.instret, cpu.time), (count, count));
                if backend == "direct" {
                    assert!(cpu.jit_stats().unwrap().specialized_loads > 0);
                }
            }
        }
    }
}

#[cfg(feature = "jit")]
#[test]
fn direct_profiled_read_group_checks_both_pages() {
    // The first read has the higher offset. After training, it stays
    // on the predicted page while the second read moves below RAM.
    let words: [u32; 9] = [
        0x01043283, 0x00043303, 0xfff50513, 0xfe051ae3, 0x00048413, 0x01000513, 0xfff58593,
        0xfe0592e3, 0x00093383,
    ];
    for backend in ["", "cranelift", "direct", "direct-mmap"] {
        for opt in [false, true] {
            let mem = backend_ram(backend, 0x3000);
            let bytes: Vec<u8> = words.iter().flat_map(|v| v.to_le_bytes()).collect();
            mem.write_bytes(0x1000, &bytes);
            mem.write_bytes(8, &39u64.to_le_bytes());
            mem.write_bytes(24, &17u64.to_le_bytes());
            let mut cpu = Cpu::new(mem);
            cpu.pc = RAM_BASE + 0x1000;
            cpu.x[8] = RAM_BASE + 8;
            cpu.x[9] = RAM_BASE - 8;
            cpu.x[10] = 32;
            cpu.x[11] = 2;
            cpu.instret_limit = 10000;
            cpu.trap_handler = Some(Box::new(|_, trap| {
                assert_eq!(trap.pc, RAM_BASE + 0x1004);
                assert_eq!(trap.tval, RAM_BASE - 8);
                assert_eq!(trap.cause, isa::CAUSE_LOAD_PAGE_FAULT);
                Err("expected fault".into())
            }));
            if !backend.is_empty() {
                cpu.enable_jit(16, 0).unwrap();
                cpu.set_jit_backend(backend).unwrap();
                cpu.set_jit_optimize(opt);
            }
            assert_eq!(cpu.run().unwrap_err(), "expected fault");
            assert_eq!((cpu.x[5], cpu.x[6]), (39, 39));
            assert_eq!((cpu.instret, cpu.time), (134, 134));
        }
    }
}

#[cfg(feature = "jit")]
#[test]
fn direct_loop_reads_revalidate_on_reentry() {
    // Each inner loop crosses the JIT timeslice limit. The outer loop
    // changes the base before entering that same header again. A range
    // miss must preserve the header arithmetic and individual fault order.
    let words: [u32; 12] = [
        0x00138393, // addi x7,x7,1
        0x01100463, // beq x0,x17,+8: optionally skip the first read
        0x00043283, // ld x5,0(x8)
        0x01043303, // ld x6,16(x8)
        0x00560633, // add x12,x12,x5
        0xfff50513, // addi x10,x10,-1
        0xfe0514e3, // bne x10,x0,-24
        0x00048413, // addi x8,x9,0
        0x00068513, // addi x10,x13,0
        0xfff58593, // addi x11,x11,-1
        0xfc059ce3, // bne x11,x0,-40
        0x00093a03, // ld x20,0(x18): final fault
    ];
    for backend in ["", "cranelift", "direct", "direct-mmap"] {
        for opt in [false, true] {
            for (crosses, skip, inner_entry) in [
                (false, false, false),
                (true, false, false),
                (false, true, false),
                (true, true, false),
                (false, false, true),
            ] {
                let mem = backend_ram(backend, 0x3000);
                let mut program = Vec::new();
                if inner_entry {
                    // An external edge into the first read must not bypass
                    // an unexecuted header guard in an optimized region.
                    program.push(0x00c0006f); // jal x0,+12
                }
                program.extend(words);
                mem.write_bytes(
                    0,
                    &program
                        .iter()
                        .flat_map(|v| v.to_le_bytes())
                        .collect::<Vec<_>>(),
                );
                for (offset, value) in [(0x2000, 17u64), (0x2010, 39), (0x2fe0, 83), (0x2ff0, 107)]
                {
                    mem.write_bytes(offset, &value.to_le_bytes());
                }
                let mut cpu = Cpu::new(mem);
                cpu.pc = RAM_BASE;
                cpu.x[8] = RAM_BASE + 0x2000;
                cpu.x[9] = RAM_BASE + if crosses { 0x2ff0 } else { 0x2fe0 };
                cpu.x[10] = 5000;
                cpu.x[11] = 2;
                cpu.x[13] = 5000;
                cpu.x[17] = u64::from(!skip);
                cpu.x[18] = RAM_BASE + 0x3000;
                cpu.instret_limit = 100000;
                cpu.trap_handler = Some(Box::new(move |_, trap| {
                    let offset = if crosses { 12 } else { 44 };
                    assert_eq!(trap.pc, RAM_BASE + offset + if inner_entry { 4 } else { 0 });
                    assert_eq!(trap.tval, RAM_BASE + 0x3000);
                    assert_eq!(trap.cause, isa::CAUSE_LOAD_PAGE_FAULT);
                    Err("expected fault".into())
                }));
                if !backend.is_empty() {
                    cpu.enable_jit(1, 0).unwrap();
                    cpu.set_jit_backend(backend).unwrap();
                    cpu.set_jit_optimize(opt);
                }
                assert_eq!(cpu.run().unwrap_err(), "expected fault");
                assert_eq!(
                    cpu.x[5],
                    if skip {
                        0
                    } else if crosses {
                        107
                    } else {
                        83
                    }
                );
                assert_eq!(cpu.x[6], if crosses { 39 } else { 107 });
                let headers = if crosses { 5001 } else { 10000 };
                assert_eq!(cpu.x[7], headers - u64::from(inner_entry));
                assert_eq!(
                    cpu.x[12],
                    if skip {
                        0
                    } else if crosses {
                        85000
                    } else {
                        500000
                    }
                );
                let count = if crosses {
                    if skip {
                        30007
                    } else {
                        35008
                    }
                } else if skip {
                    60009
                } else {
                    70009
                };
                let count = count - u64::from(inner_entry);
                assert_eq!((cpu.instret, cpu.time), (count, count));
                if backend == "direct" && !inner_entry {
                    assert!(cpu.jit_stats().unwrap().hoisted_read_loops > 0);
                }
            }
        }
    }
}

#[cfg(feature = "jit")]
#[test]
fn direct_peepholes_preserve_state() {
    // lui+addiw; lui+addi; auipc+addi; slli+add (both operand orders);
    // addi+slli; slli+add with x0; shifts that extend word/half/byte;
    // branch into a would-be pair; addi+faulting load (not fusible).
    let words: [u32; 25] = [
        0x80000537, 0xfff5051b, 0x123455b7, 0xfb958593, 0x00000617, 0x04960613, 0x00341693,
        0x009686b3, 0x00241713, 0x00e48733, 0xfef48793, 0x00279793, 0x00141813, 0x00080833,
        0x02041893, 0x0208d893, 0x03041293, 0x4302d293, 0x03841313, 0x03835313, 0x00000463,
        0x333333b7, 0x01338393, 0x00890e13, 0x000e3e03,
    ];
    for backend in ["", "cranelift", "direct"] {
        for opt in [false, true] {
            // Relocating the same code must relocate AUIPC, not constants.
            for offset in [0, 0x100] {
                let mem = Arc::new(Memory::new(RAM_BASE, 0x4000));
                let bytes: Vec<u8> = words.iter().flat_map(|v| v.to_le_bytes()).collect();
                mem.write_bytes(offset, &bytes);
                let mut cpu = Cpu::new(mem);
                let pc = RAM_BASE + offset;
                cpu.pc = pc;
                let (a, b): (u64, u64) = (0x81234567fedcba98, 0x3567);
                cpu.x[8] = a;
                cpu.x[9] = b;
                cpu.x[18] = RAM_BASE + 0x4000;
                cpu.x[7] = 5;
                cpu.instret_limit = 1000;
                cpu.trap_handler = Some(Box::new(move |_, trap| {
                    assert_eq!((trap.pc, trap.tval), (pc + 96, RAM_BASE + 0x4008));
                    assert_eq!(trap.cause, isa::CAUSE_LOAD_PAGE_FAULT);
                    Err("expected fault".into())
                }));
                if !backend.is_empty() {
                    cpu.enable_jit(1, 0).unwrap();
                    cpu.set_jit_backend(backend).unwrap();
                    cpu.set_jit_optimize(opt);
                }
                assert_eq!(cpu.run().unwrap_err(), "expected fault");
                assert_eq!(&cpu.x[10..13], &[0x7fff_ffff, 0x1234_4fb9, pc + 89]);
                assert_eq!(cpu.x[13], a.wrapping_mul(8).wrapping_add(b));
                assert_eq!(cpu.x[14], a.wrapping_mul(4).wrapping_add(b));
                assert_eq!(cpu.x[15], (b - 17) * 4);
                assert_eq!(cpu.x[16], a.wrapping_mul(2));
                assert_eq!(cpu.x[17], 0xfedc_ba98);
                assert_eq!(cpu.x[5], 0xffff_ffff_ffff_ba98);
                assert_eq!(cpu.x[6], 0x98);
                assert_eq!(
                    cpu.x[7], 24,
                    "must preserve entry to the second instruction"
                );
                assert_eq!(
                    cpu.x[28],
                    RAM_BASE + 0x4008,
                    "fault must expose intermediate address"
                );
                assert_eq!((cpu.instret, cpu.time), (24, 24));
            }
        }
    }
}

#[cfg(feature = "jit")]
#[test]
fn direct_branch_edges_preserve_state() {
    // Jump over the inner loop, then enter it through a taken side
    // exit. The outer loop follows both conditional outcomes. Trace
    // traversal order differs from region address order; 1700 inner
    // iterations also cross the native backedge instruction budget.
    let words: [u32; 10] = [
        0x0100006f, 0x00328293, 0xfff30313, 0xfe031ce3, 0x00138393, 0xfe0318e3, 0x00740413,
        0x00938463, 0xff1ff06f, 0x0005b503,
    ];
    for backend in ["", "cranelift", "direct"] {
        for opt in [false, true] {
            for inner in [1, 1700] {
                let mem = Arc::new(Memory::new(RAM_BASE, 0x4000));
                let bytes: Vec<u8> = words.iter().flat_map(|v| v.to_le_bytes()).collect();
                mem.write_bytes(0, &bytes);
                let mut cpu = Cpu::new(mem);
                cpu.pc = RAM_BASE;
                cpu.x[5] = 13;
                cpu.x[6] = inner;
                cpu.x[8] = 19;
                cpu.x[9] = 31;
                cpu.x[11] = RAM_BASE + 0x4000;
                cpu.instret_limit = 10_000;
                cpu.trap_handler = Some(Box::new(|_, trap| {
                    assert_eq!(trap.pc, RAM_BASE + 36);
                    assert_eq!(trap.tval, RAM_BASE + 0x4000);
                    assert_eq!(trap.cause, isa::CAUSE_LOAD_PAGE_FAULT);
                    Err("expected fault".into())
                }));
                if !backend.is_empty() {
                    cpu.enable_jit(1, 0).unwrap();
                    cpu.set_jit_backend(backend).unwrap();
                    cpu.set_jit_optimize(opt);
                }
                assert_eq!(cpu.run().unwrap_err(), "expected fault");
                assert_eq!(&cpu.x[5..9], &[13 + 3 * inner, 0, 31, 229]);
                assert_eq!((cpu.instret, cpu.time), (3 * inner + 153, 3 * inner + 153));
            }
        }
    }
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
