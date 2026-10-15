//! Full system emulation: memory map, devices, device tree, and boot.

pub mod config;

pub use config::*;

use crate::cpu::{Cpu, SystemMap};
use crate::dev::{self, Clint, GoldfishRtc, Htif, Plic, Syscon, Uart, UartConsole, UartNmea};
use crate::fdt::Fdt;
use crate::isa::{elf, reg, Exception, Mode};
use crate::memory::{Memory, RAM_BASE};
use crate::mmu::Mmio;
use crate::virtio::{self, VirtioMmio};
use crate::Params;
use std::fs::OpenOptions;
use std::sync::atomic::Ordering;
use std::sync::Arc;

pub const OFS_BIOS: u64 = 0x8000_0000;
pub const OFS_KERNEL: u64 = 0x8020_0000;
pub const OFS_DTB: u64 = 0x8220_0000;
pub const OFS_INITRD: u64 = 0x8800_0000;

pub const PLIC_BASE: u64 = 0x0c00_0000;

pub const CLINT_BASE: u64 = 0x200_0000;
pub const CLINT_SIZE: u64 = 0x10000;

pub const UART_BASE: u64 = 0x1000_0000;
pub const UART_IRQ: u32 = 10;

pub const SYSCON_BASE: u64 = 0x1000_0100;
pub const SYSCON_SIZE: u64 = 256;

pub const NMEA_BASE: u64 = 0x1000_0200;
pub const NMEA_IRQ: u32 = 11;

pub const VIRTIO_RAM_BASE: u64 = 0x1000_8000;
pub const VIRTIO_IRQ_BASE: u32 = 1;

pub const RTC_BASE: u64 = 0x1010_0000;
pub const RTC_SIZE: u64 = 0x1000;
pub const RTC_IRQ: u32 = 12;

pub const SHM_BASE: u64 = 0x4000_0000;

/// The system bus dispatching physical addresses to the devices.
#[derive(Default)]
pub struct Bus {
    pub segments: Vec<Box<dyn Mmio>>,
}

impl Bus {
    pub fn add<M: Mmio + 'static>(&mut self, m: M) {
        self.segments.push(Box::new(m));
    }
}

impl Mmio for Bus {
    fn contains(&self, paddr: u64, size: u64) -> bool {
        self.segments.iter().any(|s| s.contains(paddr, size))
    }

    fn load(&mut self, now: u64, paddr: u64, size: u64) -> Result<u64, Exception> {
        for seg in self.segments.iter_mut() {
            if seg.contains(paddr, size) {
                return seg.load(now, paddr, size);
            }
        }
        Err(Exception::trap(crate::isa::CAUSE_LOAD_PAGE_FAULT, paddr))
    }

    fn store(&mut self, now: u64, paddr: u64, size: u64, v: u64) -> Result<(), Exception> {
        for seg in self.segments.iter_mut() {
            if seg.contains(paddr, size) {
                return seg.store(now, paddr, size, v);
            }
        }
        Err(Exception::trap(crate::isa::CAUSE_STORE_PAGE_FAULT, paddr))
    }

    fn halt(&mut self) -> Result<(), String> {
        for seg in self.segments.iter_mut() {
            if let Err(e) = seg.halt() {
                println!("halt: {}", e);
            }
        }
        Ok(())
    }
}

/// A virtio device node for the device tree.
struct VirtioNode {
    name: String,
    start: u64,
    end: u64,
    irq: u32,
}

/// Runs the full system emulation. The `args` are additional disk
/// image files attached as virtio-blk devices.
pub fn system_emulation(
    htif: bool,
    params: &Params,
    cfg: &SystemConfig,
    args: &[String],
) -> Result<(), String> {
    let ram_size = parse_mem(&cfg.memory)?;
    let mem = Arc::new(Memory::new(RAM_BASE, ram_size));

    let mut cpu = Cpu::new(mem.clone());
    cpu.trace = params.cpu_trace;
    cpu.color = params.color;
    if params.jit {
        enable_jit(&mut cpu)?;
    }
    let signals = cpu.signals.clone();

    let plic = Plic::new(vec![signals.clone()], PLIC_BASE);

    let console: Arc<dyn dev::UartPeer> = Arc::new(UartConsole::new(params.cooked));
    let uart = Uart::new("Cons", UART_BASE, plic.clone(), UART_IRQ, console);
    let nmea = Uart::new(
        "NMEA",
        NMEA_BASE,
        plic.clone(),
        NMEA_IRQ,
        Arc::new(UartNmea::new()),
    );
    let uarts = vec![uart.clone(), nmea.clone()];

    let mut bus = Bus::default();
    bus.add(plic.clone());
    bus.add(uart);
    bus.add(nmea);
    bus.add(Syscon::new(signals.clone(), SYSCON_BASE, SYSCON_SIZE));
    bus.add(GoldfishRtc::new(RTC_BASE, RTC_SIZE));
    bus.add(Clint::new(signals.clone(), CLINT_BASE, CLINT_SIZE));

    // VirtIO devices.
    let mut virtio_devices: Vec<VirtioMmio> = Vec::new();
    let mut virtio_ram = VIRTIO_RAM_BASE;
    let mut virtio_irq = VIRTIO_IRQ_BASE;
    let mut shm_base = SHM_BASE;

    let mut add_virtio = |bus: &mut Bus, vio: VirtioMmio, virtio_ram: &mut u64, virtio_irq: &mut u32| {
        *virtio_ram = vio.end;
        *virtio_irq += 1;
        bus.add(vio.clone());
        virtio_devices.push(vio);
    };

    // Entropy source.
    let rng = virtio::Rng::create(virtio_ram, plic.clone(), virtio_irq, mem.clone())?;
    add_virtio(&mut bus, rng, &mut virtio_ram, &mut virtio_irq);

    let mut gpu: Option<virtio::GpuHandle> = None;

    for (idx, d) in cfg.devices.iter().enumerate() {
        let vio = match d.typ.as_str() {
            "virtio-blk-device" => {
                let drive = cfg
                    .drive(&d.drive)
                    .ok_or_else(|| format!("unknown drive: {}", d.drive))?;
                let file = OpenOptions::new()
                    .read(true)
                    .write(!drive.readonly)
                    .open(&drive.file)
                    .map_err(|e| format!("{}: {}", drive.file, e))?;
                let id = if d.id.is_empty() {
                    format!("goemu-disk-{}", idx)
                } else {
                    d.id.clone()
                };
                virtio::Blk::create(
                    virtio_ram,
                    plic.clone(),
                    virtio_irq,
                    mem.clone(),
                    file,
                    drive.readonly,
                    &id,
                )?
            }
            "virtio-net-device" => {
                let netdev = cfg
                    .netdev(&d.netdev)
                    .ok_or_else(|| format!("unknown netdev: {}", d.netdev))?;
                virtio::Net::create(virtio_ram, plic.clone(), virtio_irq, mem.clone(), netdev)
                    .map_err(|e| format!("failed to create virtio-net-device: {}", e))?
            }
            "virtio-gpu-device" => {
                if cfg.nographic {
                    continue;
                }
                let gpudev = cfg
                    .gpu(&d.gpu)
                    .ok_or_else(|| format!("unknown GPU: {}", d.gpu))?;
                let (vio, handle) =
                    virtio::Gpu::create(virtio_ram, plic.clone(), virtio_irq, mem.clone(), gpudev)?;
                shm_base = vio.assign_shm(shm_base);
                for shm in vio.shm_segments() {
                    println!("GPU.SHM: [{:x}-{:x}[", shm.start, shm.end);
                    bus.add(shm);
                }
                gpu = Some(handle);
                vio
            }
            t => return Err(format!("invalid device type: {}", t)),
        };
        add_virtio(&mut bus, vio, &mut virtio_ram, &mut virtio_irq);
    }

    // If a GPU is configured, add its keyboard and pointer devices.
    if let Some(g) = gpu.as_mut() {
        let kbd = virtio::Input::create(
            virtio_ram,
            plic.clone(),
            virtio_irq,
            mem.clone(),
            g.width,
            g.height,
            virtio::InputType::Keyboard,
        )?;
        g.set_keyboard(kbd.clone());
        add_virtio(&mut bus, kbd.mmio(), &mut virtio_ram, &mut virtio_irq);

        let ptr = virtio::Input::create(
            virtio_ram,
            plic.clone(),
            virtio_irq,
            mem.clone(),
            g.width,
            g.height,
            g.pointer,
        )?;
        g.set_pointer(ptr.clone());
        add_virtio(&mut bus, ptr.mmio(), &mut virtio_ram, &mut virtio_irq);
    }

    // Remaining arguments are disk image files.
    for (idx, arg) in args.iter().enumerate() {
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .open(arg)
            .map_err(|e| format!("failed to open disk file {}: {}", arg, e))?;
        let blk = virtio::Blk::create(
            virtio_ram,
            plic.clone(),
            virtio_irq,
            mem.clone(),
            file,
            false,
            &format!("arg-disk-{}", idx),
        )?;
        add_virtio(&mut bus, blk, &mut virtio_ram, &mut virtio_irq);
    }
    drop(add_virtio);

    cpu.bus = Some(Box::new(bus));
    cpu.set_mode(Mode::M);

    if !cfg.symbols.is_empty() {
        cpu.symtab = Some(SystemMap::load(&cfg.symbols)?);
    }

    let mut entrypoint = 0;
    if !cfg.bios.is_empty() {
        let data =
            std::fs::read(&cfg.bios).map_err(|e| format!("failed to read BIOS: {}", e))?;
        load_ram(&mem, OFS_BIOS, &data)?;
        entrypoint = OFS_BIOS;
    }

    let mut htif_status = None;
    if !cfg.kernel.is_empty() {
        let (htif_dev, entry) = load_kernel(&cpu, &mem, &cfg.kernel)
            .map_err(|e| format!("failed to read kernel: {}", e))?;
        if cfg.bios.is_empty() {
            entrypoint = entry;
        }
        if htif {
            let h = htif_dev.ok_or_else(|| "no HTIF device".to_string())?;
            htif_status = Some(h.exit_status.clone());
            cpu.overlay = Some(Box::new(h));
        }
    }

    let mut initrd_size = 0;
    if !cfg.initrd.is_empty() {
        let data =
            std::fs::read(&cfg.initrd).map_err(|e| format!("failed to read initrd: {}", e))?;
        initrd_size = data.len() as u64;
        load_ram(&mem, OFS_INITRD, &data)?;
    }

    let nodes: Vec<VirtioNode> = virtio_devices
        .iter()
        .map(|v| VirtioNode {
            name: v.name.clone(),
            start: v.start,
            end: v.end,
            irq: v.irq,
        })
        .collect();
    let dtb = make_dtb(initrd_size, &mem, &plic, &uarts, &nodes, cfg);
    if !cfg.dumpdtb.is_empty() {
        std::fs::write(&cfg.dumpdtb, &dtb).map_err(|e| format!("{}: {}", cfg.dumpdtb, e))?;
    }
    load_ram(&mem, OFS_DTB, &dtb)?;

    cpu.x[reg::A0 as usize] = 0;
    cpu.x[reg::A1 as usize] = OFS_DTB;
    cpu.pc = entrypoint;

    // Start UARTs.
    for u in &uarts {
        u.start_peer();
    }

    let (instret, runtime, jit_stats) = if let Some(g) = gpu {
        // The display event loop runs on the main thread.
        let handle = std::thread::Builder::new()
            .name("cpu".to_string())
            .spawn(move || {
                let r = cpu.run();
                (r, cpu.instret, cpu.runtime, jit_stats(&cpu))
            })
            .map_err(|e| e.to_string())?;
        g.event_loop(signals.clone());
        signals.shutdown();
        let (r, instret, runtime, stats) = handle.join().map_err(|_| "CPU thread panicked")?;
        r?;
        (instret, runtime, stats)
    } else {
        cpu.run()?;
        (cpu.instret, cpu.runtime, jit_stats(&cpu))
    };

    let secs = runtime.as_secs_f64();
    println!(
        "CPU: instret: {}, runtime: {:?}, MIPS: {:.2}",
        instret,
        runtime,
        if secs > 0.0 {
            instret as f64 / 1e6 / secs
        } else {
            0.0
        }
    );
    if let Some(s) = jit_stats {
        println!("{}", s);
    }
    for v in &virtio_devices {
        v.stats();
    }
    if let Some(status) = htif_status {
        let s = status.load(Ordering::SeqCst);
        if s != 1 {
            println!("HTIF: assertion {}", s >> 1);
        }
    }
    Ok(())
}

#[cfg(feature = "jit")]
fn enable_jit(cpu: &mut Cpu) -> Result<(), String> {
    cpu.enable_jit(crate::cpu::DEFAULT_HOT_THRESHOLD)
}

#[cfg(not(feature = "jit"))]
fn enable_jit(_cpu: &mut Cpu) -> Result<(), String> {
    Err("JIT support not compiled in (enable the `jit` feature)".to_string())
}

#[cfg(feature = "jit")]
fn jit_stats(cpu: &Cpu) -> Option<String> {
    cpu.jit_stats().map(|s| s.to_string())
}

#[cfg(not(feature = "jit"))]
fn jit_stats(_cpu: &Cpu) -> Option<String> {
    None
}

fn load_ram(mem: &Memory, paddr: u64, data: &[u8]) -> Result<(), String> {
    if !mem.contains_range(paddr, data.len() as u64) {
        return Err(format!(
            "image of {} bytes at {:x} does not fit in RAM",
            data.len(),
            paddr
        ));
    }
    mem.write_bytes(mem.offset(paddr), data);
    Ok(())
}

/// Loads the kernel image. Raw images are loaded at OFS_KERNEL; ELF
/// images at their physical load addresses. Returns the HTIF device
/// if the ELF image defines the tohost symbol, and the entry point.
fn load_kernel(cpu: &Cpu, mem: &Memory, file: &str) -> Result<(Option<Htif>, u64), String> {
    let data = std::fs::read(file).map_err(|e| format!("{}: {}", file, e))?;
    if !elf::is_elf(&data) {
        load_ram(mem, OFS_KERNEL, &data)?;
        return Ok((None, OFS_KERNEL));
    }

    let f = elf::File::parse(data)?;
    println!("ELF: entry={:x}", f.entry);
    for prog in &f.progs {
        if prog.typ != elf::PT_LOAD {
            continue;
        }
        println!("ELF: loading {} bytes to {:x}", prog.memsz, prog.paddr);
        if !mem.contains(prog.paddr) || !mem.contains(prog.paddr + prog.memsz - 1) {
            return Err(format!(
                "prog out of range: {:x}...{:x}",
                prog.paddr,
                prog.paddr + prog.memsz
            ));
        }
        load_ram(mem, prog.paddr, f.prog_data(prog))?;
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
        return Ok((None, f.entry));
    }
    let (start, size) = if to.0 < from.0 {
        (to.0, from.0 + from.1 - to.0)
    } else {
        (from.0, to.0 + to.1 - from.0)
    };
    Ok((
        Some(Htif::new(cpu.signals.clone(), start, size, to.0, from.0)),
        f.entry,
    ))
}

fn make_dtb(
    initrd_size: u64,
    mem: &Memory,
    plic: &Plic,
    uarts: &[Uart],
    virtio_devices: &[VirtioNode],
    cfg: &SystemConfig,
) -> Vec<u8> {
    let mut fdt = Fdt::new();

    // Root node.
    fdt.begin_node("");
    fdt.prop_str("model", "goemu,riscv-emulator");
    fdt.prop_str("compatible", "riscv,virtio");
    fdt.prop_u32("#address-cells", 2);
    fdt.prop_u32("#size-cells", 2);

    // CPUs.
    fdt.begin_node("cpus");
    fdt.prop_u32("#address-cells", 2);
    fdt.prop_u32("#size-cells", 0);
    fdt.prop_u32("timebase-frequency", 100_000_000);

    fdt.begin_node("cpu@0");
    fdt.prop_str("device_type", "cpu");
    fdt.prop_str("status", "okay");
    fdt.prop_u32s("reg", &[0, 0]);
    fdt.prop_str("compatible", "riscv");
    fdt.prop_u32("clock-frequency", 100_000_000);
    fdt.prop_str("riscv,isa", "rv64gcv_sstc");
    fdt.prop_str("riscv,isa-base", "rv64i");
    fdt.prop_strs(
        "riscv,isa-extensions",
        &[
            "i", "m", "a", "f", "d", "c", "v", "zicsr", "zifencei", "zicntr", "zihpm", "sstc",
        ],
    );
    fdt.prop_str("mmu-type", "riscv,sv39");

    // CPU local interrupt controller.
    fdt.begin_node("interrupt-controller");
    fdt.prop_u32("#interrupt-cells", 1);
    fdt.prop_empty("interrupt-controller");
    fdt.prop_str("compatible", "riscv,cpu-intc");
    fdt.prop_u32("phandle", 1);
    fdt.end_node(); // interrupt-controller

    fdt.end_node(); // cpu@0
    fdt.end_node(); // cpus

    // RAM.
    fdt.begin_node_num("memory", mem.ram_base);
    fdt.prop_str("device_type", "memory");
    fdt.prop_u64_pair("reg", RAM_BASE, mem.ram_end - mem.ram_base);
    fdt.end_node();

    // SoC peripherals bus.
    fdt.begin_node("soc");
    fdt.prop_str("compatible", "simple-bus");
    fdt.prop_u32("#address-cells", 2);
    fdt.prop_u32("#size-cells", 2);
    fdt.prop_empty("ranges");

    // CLINT: machine software (3) and timer (7) interrupts.
    fdt.begin_node_num("clint", CLINT_BASE);
    fdt.prop_str("compatible", "riscv,clint0");
    fdt.prop_u64_pair("reg", CLINT_BASE, CLINT_SIZE);
    fdt.prop_u32s("interrupts-extended", &[1, 3, 1, 7]);
    fdt.end_node();

    // PLIC: machine (11) and supervisor (9) external interrupts.
    fdt.begin_node_num("plic", plic.start);
    fdt.prop_str("compatible", "sifive,plic-1.0.0");
    fdt.prop_u64_pair("reg", plic.start, plic.end - plic.start);
    fdt.prop_u32("#interrupt-cells", 1);
    fdt.prop_empty("interrupt-controller");
    fdt.prop_u32("riscv,ndev", plic.max_interrupts);
    fdt.prop_u32("phandle", 2);
    fdt.prop_u32s("interrupts-extended", &[1, 11, 1, 9]);
    fdt.end_node();

    // UARTs.
    for u in uarts {
        fdt.begin_node_num("uart", u.start);
        fdt.prop_str("compatible", "ns16550a");
        fdt.prop_u64_pair("reg", u.start, u.end - u.start);
        fdt.prop_u32s("clock-frequency", &[24_000_000]);
        fdt.prop_u32("reg-shift", 0);
        fdt.prop_u32("reg-io-width", 1);
        fdt.prop_u32s("interrupts-extended", &[2, u.irq]);
        fdt.end_node();
    }

    // Syscon register block with poweroff and reboot controllers.
    fdt.begin_node_num("syscon", SYSCON_BASE);
    fdt.prop_strs("compatible", &["syscon", "simple-mfd"]);
    fdt.prop_u64_pair("reg", SYSCON_BASE, SYSCON_SIZE);
    fdt.prop_u32("phandle", 3);

    fdt.begin_node("poweroff");
    fdt.prop_str("compatible", "syscon-poweroff");
    fdt.prop_u32("regmap", 3);
    fdt.prop_u32("offset", 0);
    fdt.prop_u32("value", dev::POWEROFF_MAGIC);
    fdt.prop_u32("priority", 100);
    fdt.end_node();

    fdt.begin_node("reboot");
    fdt.prop_str("compatible", "syscon-reboot");
    fdt.prop_u32("regmap", 3);
    fdt.prop_u32("offset", 4);
    fdt.prop_u32("value", dev::REBOOT_MAGIC);
    fdt.prop_u32("priority", 200);
    fdt.end_node();

    fdt.end_node(); // syscon

    // Goldfish RTC.
    fdt.begin_node_num("rtc", RTC_BASE);
    fdt.prop_str("compatible", "google,goldfish-rtc");
    fdt.prop_u64_pair("reg", RTC_BASE, RTC_SIZE);
    fdt.prop_u32s("interrupts-extended", &[2, RTC_IRQ]);
    fdt.end_node();

    // VirtIO devices.
    for d in virtio_devices {
        fdt.begin_node_num(&d.name, d.start);
        fdt.prop_str("compatible", "virtio,mmio");
        fdt.prop_u64_pair("reg", d.start, d.end - d.start);
        fdt.prop_u32s("interrupts-extended", &[2, d.irq]);
        fdt.end_node();
    }

    fdt.end_node(); // soc

    // Chosen.
    fdt.begin_node("chosen");
    fdt.prop_str("bootargs", &cfg.append);
    if initrd_size > 0 {
        fdt.prop_u64("linux,initrd-start", OFS_INITRD);
        fdt.prop_u64("linux,initrd-end", OFS_INITRD + initrd_size);
    }
    fdt.prop_str("stdout-path", "/soc/uart@10000000:115200n8");
    fdt.end_node();

    fdt.end_node(); // root

    fdt.output()
}
