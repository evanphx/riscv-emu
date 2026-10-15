# RISC-V emulator in Rust

A Rust implementation of [markkurossi/riscv](https://github.com/markkurossi/riscv),
an RV64GC emulator that boots Linux through OpenSBI, with Sv39 virtual
memory, VirtIO devices, a Linux userspace (syscall) emulation mode,
and an optional [Cranelift](https://cranelift.dev) JIT compiler.

```text
OpenSBI → Linux → buildroot userspace
OpenSBI → U-Boot
```

## Features

- RV64IMAFDC with Zicsr, Zifencei, Zicntr, Zicond, Zba, Zbb, Zbc, Zbs,
  Zbkb, Zbkx, Zicboz, Zcb, Sstc, and the vector subset of the original
- IEEE 754 single and double precision with all rounding modes and
  exception flags (software floating point, no host FPU dependence)
- Machine, Supervisor, and User modes; Sv39 page tables with hardware
  A/D bit updates, MPRV, SUM, and MXR
- Device tree generation, OpenSBI and U-Boot boot
- Devices: NS16550A UARTs (console and an NMEA GPS clock), PLIC, CLINT
  (ACLINT MSWI/MTIMER), Goldfish RTC, syscon poweroff/reboot, HTIF
- VirtIO MMIO devices: block, entropy, network (TUN, built-in DHCP
  server), GPU and input
- Linux userspace emulation for static and dynamic RISC-V binaries
- Cranelift JIT (`-jit`) for hot guest code
- `riscv-objdump` ELF disassembler

## Building

```shell
$ cargo build --release
```

This builds `target/release/rsemu` and `target/release/riscv-objdump`.
The JIT is the default `jit` cargo feature; build with
`--no-default-features` for an interpreter-only binary.

## Booting Linux

The guest images (OpenSBI, Linux kernel, buildroot root filesystem)
live in the upstream repository's `cmd/goemu` directory. The
configuration files in [configs](configs) use the same JSON format as
the original `.goemu` files and paths relative to the current
directory:

```shell
$ git clone https://github.com/markkurossi/riscv
$ cd riscv/cmd/goemu
$ /path/to/rsemu -jit /path/to/riscv-rs/configs/buildroot.goemu
OpenSBI v1.8.1-126-ga8be5e94
...
Welcome to GoEMU RISC-V Linux

goemu login: root
# uname -a
Linux goemu 7.0.11 #9 SMP PREEMPT Wed Jun  3 08:23:43 UTC 2026 riscv64 GNU/Linux
# poweroff
...
[   27.320208] reboot: Power down
CPU: instret: 1502759037, runtime: 18.642273947s, MIPS: 80.61
JIT: 2199 blocks (57826 instructions) compiled in 2.205362368s, 125726884 block runs, 1349007312 instructions executed, 0 invalidated
virtio-rng: read    : 576 B
virtio-blk: read    : 3 MB
virtio-blk: wrote   : 0 B
```

Configuration values can also be given or overridden on the command
line, in the style of the original:

```shell
$ rsemu -bios opensbi/fw_jump.bin \
        -kernel linux-7.0.11/Image \
        -symbols linux-7.0.11/System.map \
        -drive id=hd0,format=raw,file=rootfs.ext2 \
        -device virtio-blk-device,drive=hd0 \
        -append "earlycon=sbi console=ttyS0,115200 root=/dev/vda ro rootwait"
```

Main flags:

| Flag | Description |
|------|-------------|
| `-bios file` | firmware, loaded at 0x80000000 |
| `-kernel file` | kernel or ELF image (raw images at 0x80200000) |
| `-initrd file` | initial ramdisk |
| `-append args` | kernel command line |
| `-m size` | RAM size, e.g. `512M` (default) or `2G` |
| `-drive` / `-device` / `-netdev` | drives, devices, and network devices |
| `-gpu id=gpu0,width=1024,height=768` | add a VirtIO GPU with keyboard and pointer |
| `-nographic` | ignore GPU devices |
| `-jit` | enable the JIT compiler |
| `-jit-threshold n` | executions before a block is compiled (default 3000) |
| `-jit-sync` | compile blocks synchronously, for deterministic runs |
| `-jit-cache file` | load and save JIT compiled code in `file` (implies `-jit`) |
| `-cooked` | don't put the terminal into raw mode |
| `-htif` | enable the HTIF tohost/fromhost device for ELF kernels |
| `-symbols file` | kernel System.map for traces |
| `-cputrace` | trace traps and privilege transitions |
| `-dumpdtb file` | write the generated device tree |
| `-D file` | disassemble an ELF file |

Additional arguments that are not `.goemu` files are attached as
VirtIO block devices.

## Networking

A `virtio-net-device` connects the guest to a host TUN interface. The
emulator answers the guest's ARP and DHCP requests itself and forwards
the other traffic to the TUN interface, whose address is the
netdev's gateway. Creating the interface needs root:

```shell
$ sudo rsemu -netdev id=eth0,ip=192.168.42.4,gw=192.168.42.254 \
             -device virtio-net-device,netdev=eth0 buildroot.goemu
# udhcpc -i eth0
# ping 192.168.42.254
```

To reach other networks, enable IP forwarding and NAT for the guest
subnet on the host.

## Graphics

`-gpu` (or a `gpus` entry and a `virtio-gpu-device` in the
configuration) adds a VirtIO GPU with a keyboard and a pointer device
and opens a window (winit + softbuffer, the default `gui` cargo
feature). The guest kernel needs the virtio-gpu DRM driver and
`console=tty0` for a framebuffer console; the buildroot kernel image in
the upstream repository is built without it. Click the window to grab
the mouse and press Alt+Esc to release it. Without a display, the GPU
still works and the emulator logs `virtio-gpu: no display`.

## Linux userspace emulation

Without `-bios`/`-kernel`, `rsemu` runs a RISC-V Linux ELF binary
directly and emulates the Linux system calls:

```shell
$ rsemu examples/hello
Hello, RISC-V!
$ rsemu -jit static/fibo x
fibo(35): 9227465
$ rsemu -ktrace -fsroot image dynamic/uname   # ld.so is loaded from the fsroot
```

The C sources and makefiles of the test programs are in
[guest/tests](guest/tests); build them with a RISC-V cross compiler
such as `riscv64-linux-gnu-gcc`.

## The JIT

With `-jit`, guest code that has run `-jit-threshold` times (3000 by
default) is compiled to native code with Cranelift on a background
thread while the interpreter keeps running:

- A compiled unit is a trace: it starts at a guest PC and follows
  not-taken conditional branches and direct jumps within the guest
  page, with side exits for taken branches. A trace that jumps back to
  its start loops in native code.
- Traces are keyed by the virtual PC and the physical location of the
  code. Guest registers are held in SSA values and written through to
  the CPU state.
- The generated code computes guest PC values relative to the trace
  start, so it depends only on the code bytes and their page offset.
  When the same code shows up at another address (a shared library
  mapped at a different address in each process because of the guest's
  ASLR, or code loaded into another physical page), the existing
  machine code is reused instead of being compiled again.
- Loads and stores inline the TLB lookup and access guest RAM directly
  on a hit; misses, MMIO, and faults call back into the CPU, so the
  memory model and exceptions are identical to the interpreter.
- Floating point loads, stores, and moves are compiled natively; other
  floating point, atomic, and vector instructions call the interpreter
  from compiled code. CSR, system, and privileged instructions end the
  trace and run in the interpreter.
- Each trace returns the number of instructions it executed, so time,
  `instret`, and the interrupt checks advance exactly as in the
  interpreter.
- `fence.i` and CPU stores to pages that contain compiled code start a
  new epoch; traces from an older epoch are checked against the
  current guest code before they run again.

The machine code is generated with `cranelift-codegen` and copied into
the emulator's own executable code arena. The JIT supports x86-64
hosts. `-jit-sync` compiles blocks before running them instead of in
the background, which makes runs deterministic and is useful for
debugging.

### Saving compiled code

`-jit-cache file` saves the compiled code when the emulator exits and
loads it at the next start, so the kernel and the programs of the
previous runs run as native code from their first instruction instead
of being interpreted until they become hot:

```
$ rsemu -jit-cache linux.jit configs/buildroot.goemu   # first run compiles and saves
$ rsemu -jit-cache linux.jit configs/buildroot.goemu   # later runs load the code
```

To precompile everything that a run executes, not just the hot code,
make one training run with `-jit-sync -jit-threshold 2`; later runs can
use the default settings.

- The code refers to host addresses (guest RAM, the TLB, helper
  functions) through relocations, which are patched when the code is
  loaded.
- Loaded code is checked against the guest code before it first runs,
  just like code from an older `fence.i` epoch, so a cache from another
  kernel or disk image is safe to use; blocks that do not match are
  compiled again.
- The cache is tied to the emulator binary and the host CPU features.
  A cache from another build is ignored and replaced at exit. A file
  that is not a code cache is never overwritten.
- The file keeps the code that ran in the last runs; code that has not
  been used in 4 runs is dropped.
- The cache is written when the CPU stops (guest poweroff or program
  exit).

### Performance

Host wall-clock times on a shared 2-core x86-64 machine. The md5sum,
shell loop, and sort benchmarks run inside the booted buildroot guest.

| Workload | Go original | Rust interpreter | Rust JIT |
|----------|------------:|-----------------:|---------:|
| Linux boot to login prompt | 9.6 s | 9.7 s | 4.2 s |
| `fibo(35)` (userspace mode) | 3.48 s | 4.24 s | 0.99 s |
| `fibo(40)` (userspace mode) | 34.4 s | 38.5 s | 8.3 s |
| md5sum of 8 MB in the guest | – | 1.78 s | 0.67 s |
| shell loop, 20000 iterations | – | 4.40 s | 1.24 s |
| `seq 1 30000 \| sort -r` | – | 2.05 s | 0.75 s |

## Tests

```shell
$ cargo test --release
```

runs the unit tests and [tests/testsuite.rs](tests/testsuite.rs), which
executes the riscv-tests ISA suite and the RISC-V vector tests from
[testdata](testdata) in M-mode with the HTIF device, once in the
interpreter and once with every block JIT compiled. All 185 ISA tests
and 47 vector tests pass (the Go version passes 131 ISA tests). The
`rv64mzicbo-p-zero` binary is skipped: it is position independent and
its GOT follows the test data inside the 64-byte cache block that
`cbo.zero` clears.

## Layout

| Go package | Rust module |
|------------|-------------|
| `isa` | [src/isa](src/isa): opcodes, decoding, CSRs, traps, ELF |
| `memory`, `mmu` | [src/memory.rs](src/memory.rs), [src/mmu.rs](src/mmu.rs), [src/cpu/mmu.rs](src/cpu/mmu.rs) |
| `cpu` | [src/cpu](src/cpu): interpreter, CSRs, traps, FPU, vector unit, [JIT](src/cpu/jit.rs) |
| `dev` | [src/dev](src/dev) |
| `virtio` | [src/virtio](src/virtio) |
| `network` | [src/network](src/network) |
| `emulator`, `kernel` | [src/emulator](src/emulator), [src/kernel](src/kernel) |
| `cmd/goemu` | [src/system](src/system), [src/fdt.rs](src/fdt.rs), [src/bin/rsemu.rs](src/bin/rsemu.rs) |
| `cmd/riscv-objdump` | [src/bin/riscv-objdump.rs](src/bin/riscv-objdump.rs) |

Guest memory is shared between the CPU thread and the device threads
the same way the Go version shares it between goroutines; the pending
interrupt register is an atomic shared with the devices, and address
translation is implemented on the CPU itself.

## Differences from the Go version

Behavior follows the Go version, with these deliberate changes:

- Instructions and extensions the Go version leaves unimplemented or
  approximates are implemented to the specification: Zbb, Zbc, Zbs,
  Zbkb, Zbkx, Zcb, `cbo.zero`, fused multiply-add variants, and IEEE
  floating point with NaN boxing, rounding modes, and flags.
- The MMU writes the PTE accessed and dirty bits back to memory,
  supports MPRV, and checks SUM/MXR changes in the TLB.
- `misa` reports V instead of the reserved G bit, and `mideleg` and
  `medeleg` only accept delegable bits. OpenSBI prints these values in
  its banner.
- AMOs check alignment, and unmapped virtual address ranges outside
  Sv39 fault.
- Host profiling flags (`-cpuprofile`, CSR 0x802) are accepted but
  ignored.
- The userspace emulator stops on unhandled traps instead of
  repeating them, and its mmap area starts at a valid Sv39 user
  address.

## License

MIT, see [LICENSE](LICENSE).
