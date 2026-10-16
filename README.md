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
live in the upstream repository's `cmd/goemu` directory.
[configs/buildroot.goemu](configs/buildroot.goemu) boots the kernel
built from [guest/linux](guest/linux), which is linked next to them;
[configs/buildroot-goemu-kernel.goemu](configs/buildroot-goemu-kernel.goemu)
boots the upstream repository's kernel instead. The configuration
files use the same JSON format as the original `.goemu` files and
paths relative to the current directory:

```shell
$ git clone https://github.com/markkurossi/riscv
$ ln -s /path/to/riscv-rs/guest/linux/linux-7.0.11-rsemu riscv/cmd/goemu/
$ cd riscv/cmd/goemu
$ /path/to/rsemu -jit /path/to/riscv-rs/configs/buildroot.goemu
OpenSBI v1.8.1-126-ga8be5e94
...
Welcome to GoEMU RISC-V Linux

goemu login: root
# uname -a
Linux goemu 7.0.11 #1 SMP PREEMPT Fri Oct 16 14:42:29 UTC 2026 riscv64 GNU/Linux
# poweroff
...
[    3.970105] reboot: Power down
CPU: instret: 240546907, runtime: 3.947535688s, MIPS: 60.94
JIT: 2735 blocks (57867 instructions) compiled in 1.01795869s, 1379635 dispatches, 172073729 instructions executed, 1 invalidated, 5370 reused at other addresses
virtio-rng: read    : 320 B
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
| `-jit-threshold n` | executions before a block is compiled (default 1000; implies `-jit`) |
| `-jit-sync` | compile blocks synchronously, for deterministic runs (implies `-jit`) |
| `-jit-cache file` | load and save JIT compiled code in `file` (implies `-jit`) |
| `-jit-opt` | compile slower, optimized code, e.g. to build a `-jit-cache` (implies `-jit`) |
| `-suspend file` | suspend the system to `file` on Ctrl-A s or `SIGUSR1` |
| `-restore file` | restore a suspended system from `file` |
| `-cooked` | don't put the terminal into raw mode |
| `-htif` | enable the HTIF tohost/fromhost device for ELF kernels |
| `-symbols file` | kernel System.map for traces |
| `-cputrace` | trace traps and privilege transitions |
| `-dumpdtb file` | write the generated device tree |
| `-D file` | disassemble an ELF file |

Additional arguments that are not `.goemu` files are attached as
VirtIO block devices.

## Suspend and restore

With `-suspend file`, pressing Ctrl-A s at the console (or sending
the emulator `SIGUSR1`) stops the guest and saves the whole machine,
RAM, CPU, and device state, to `file`; `-restore file` continues it
where it stopped:

```shell
$ rsemu -jit -suspend vm.snap configs/buildroot.goemu
...
# Suspended to vm.snap: 37.2 MiB of RAM in 21.3ms
$ rsemu -jit -restore vm.snap
Restored vm.snap in 3.0ms
```

A restored system uses the configuration saved in the snapshot, so
`-restore` takes no `.goemu` files, disks, or `-m`, and it is
suspended back into the same file unless `-suspend` names another.
With suspend enabled, Ctrl-A a or Ctrl-A Ctrl-A sends a Ctrl-A to the
guest.

The snapshot is built for fast suspend and restore:

- Guest RAM is stored page-aligned in the file, so a restore maps it
  into the guest copy-on-write with one `mmap` call instead of reading
  it: restoring takes a few milliseconds regardless of the RAM size, and the
  host reads pages from the file (usually from the page cache) when
  the guest first touches them.
- Zero pages are not written and stay holes in the file, so a
  snapshot of a 512 MiB guest that uses 37 MiB takes 37 MiB of disk.
  Untouched RAM is found from `/proc/self/pagemap` without reading it,
  and the remaining pages are written by several threads in large
  runs.
- Suspending a restored VM to its own snapshot file writes back only
  the pages the guest modified since the restore (the pages
  `/proc/self/pagemap` reports as copied-on-write), which typically
  takes a few milliseconds. On filesystems with reflinks (XFS, Btrfs),
  saving to another file clones the original and also writes only the
  modified pages.
- Several emulators can restore the same snapshot at the same time.
  They hold a shared lock on it, and a VM saving to a file that
  another one has mapped writes a new file and renames it into place
  instead of modifying it.

Before saving, the devices finish their in-flight disk requests and
network transmissions; receive buffers the guest posted stay with the
guest and are handed to the device again on restore. Host input that
the guest has not read when it is suspended (console bytes, network
frames) is dropped. If the snapshot cannot be written, the error is
printed and the guest continues running.

The disk images are not part of the snapshot: the guest's disk cache
expects them to be unchanged when it is restored. The emulator
refuses a disk whose size changed and warns about one modified after
the snapshot was taken. The guest clock continues from where it
stopped. The VirtIO GPU and the HTIF test device are not supported.

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
`console=tty0` for a framebuffer console; the kernel of
`buildroot.goemu` has the driver, the upstream repository's kernel
image is built without it. Click the window to grab
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

With `-jit`, guest code that has run `-jit-threshold` times (1000 by
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
  memory model and exceptions are identical to the interpreter. The
  calls go through stubs that preserve all registers, so the rarely
  taken slow paths do not cost register moves on the fast paths. TLB
  entries for pages that hold compiled code have no direct write
  permission, so stores to code always go through the CPU, which
  invalidates the code.
- Floating point loads, stores, and moves are compiled natively; other
  floating point, atomic, and vector instructions call the interpreter
  from compiled code. CSR, system, and privileged instructions end the
  trace and run in the interpreter.
- Traces jump directly to each other: an exit to a known PC or a
  `jalr` looks up the target in a jump cache and tail-calls the next
  trace, so most transitions never return to the dispatcher. Jump cache
  entries are tagged with the privilege mode and a generation that
  changes on TLB flushes, `satp` writes, and code changes, so a jump
  never bypasses an address translation or code check that the
  dispatcher would make.
- A chain of traces runs at most 4096 instructions before returning to
  the dispatcher, which then checks for interrupts. Traces return the
  number of instructions they executed, so time and `instret` advance
  exactly as in the interpreter.
- Writes to pages that contain compiled code, by the CPU or by device
  DMA, mark the code as modified. A CPU store, or the next `fence.i`
  after a DMA write, starts a new epoch; traces from an older epoch are
  checked against the current guest code before they run again. A
  `fence.i` without modified code costs nothing, which matters because
  Linux executes about a thousand of them while booting.
- `sfence.vma` with an address flushes only that page from the TLB and
  the jump cache.

The machine code is generated with `cranelift-codegen`, by default
without its optimization passes, which keeps compiling fast, and
copied into the emulator's own executable code arena. The JIT supports
x86-64 hosts. `-jit-sync` compiles blocks before running them instead of in
the background, which makes runs deterministic and is useful for
debugging. With the environment variable `RSEMU_PERF_MAP` set, the JIT
writes `/tmp/perf-PID.map`, so that `perf report` shows compiled code
by guest PC (`rv:ffffffff8012dccc`) instead of as anonymous memory.

### Saving compiled code

`-jit-cache file` saves the compiled code when the emulator exits and
loads it at the next start, so the kernel and the programs of the
previous runs run as native code from their first instruction instead
of being interpreted until they become hot:

```
$ rsemu -jit-cache linux.jit configs/buildroot.goemu   # first run compiles and saves
$ rsemu -jit-cache linux.jit configs/buildroot.goemu   # later runs load the code
```

- The generated code is position independent: it finds host
  addresses (the TLB, helper functions, the jump cache) through the CPU
  state. The cache file is mapped into memory and its code runs in
  place, so loading a cache of 30,000 blocks takes about 25 ms.
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

### Precompiling the kernel

A training run can compile everything that a boot executes, ahead of
time and at a higher quality than the background compiler can afford:

```
$ rsemu -jit-sync -jit-threshold 1 -jit-opt -jit-cache linux.jit configs/buildroot.goemu
  (log in and power off; about 35 s)
$ rsemu -jit-cache linux.jit configs/buildroot.goemu
```

- `-jit-threshold 1` compiles code the first time it runs, so also the
  kernel's run-once initialization code ends up in the cache.
- `-jit-opt` compiles regions instead of traces: all code reachable
  from the entry within the page, following both sides of branches, up
  to 128 instructions, with native control flow inside the region, so
  loops with internal branches stay in native code. Cranelift's
  optimizations are on. Compiling is about twice as slow, which does
  not matter for a training run.
- Later runs use the default settings; code that the training run did
  not see is compiled as usual and added to the cache.

For the buildroot image the cache holds about 26,600 kernel regions and
6,500 user space regions (75 MiB). A boot with it executes 99.5% of the
instructions in loaded code; only 3 blocks are compiled before the
kernel starts init.

Time from start to the kernel's `Run /sbin/init` message (medians of
10 interleaved runs):

| Configuration | Time to init |
|---------------|-------------:|
| Interpreter | 7.97 s |
| `-jit` | 1.76 s |
| `-jit-cache`, after one normal run | 1.45 s |
| `-jit-cache`, precompiled with `-jit-sync -jit-threshold 1` | 1.29 s |
| `-jit-cache`, precompiled with `-jit-sync -jit-threshold 1 -jit-opt` | 0.98 s |

About a third of the remaining time is a single kernel function,
`trace_event_update_all`, which scans the format strings of all trace
events while booting.

### Performance

Host wall-clock times on a shared 2-core x86-64 machine. The md5sum,
shell loop, and sort benchmarks run inside the booted buildroot guest.
The Linux measurements in this section and in the JIT sections above
use the upstream repository's kernel
(`configs/buildroot-goemu-kernel.goemu`), which is the one the Go
original boots.

| Workload | Go original | Rust interpreter | Rust JIT | JIT, warm `-jit-cache` | JIT, precompiled `-jit-opt` cache |
|----------|------------:|-----------------:|---------:|-----------------------:|----------------------------------:|
| Linux boot to login prompt | 9.6 s | 9.7 s | 2.5 s | 2.0 s | 1.4 s |
| `fibo(35)` (userspace mode) | 3.48 s | 4.24 s | 0.49 s | | |
| `fibo(40)` (userspace mode) | 34.4 s | 38.5 s | 6.0 s | | |
| md5sum of 8 MB in the guest | – | 1.78 s | 0.41 s | 0.38 s | 0.27 s |
| shell loop, 20000 iterations | – | 4.40 s | 0.55 s | 0.46 s | 0.32 s |
| `seq 1 30000 \| sort -r` | – | 2.05 s | 0.37 s | 0.30 s | 0.22 s |

The JIT times are medians of three runs; the machine is shared, so
individual runs vary by about 10%.

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
