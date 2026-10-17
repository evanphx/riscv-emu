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
  server), GPU, input and sockets (vsock)
- An HTTP API that runs shell commands in VMs and continues them from
  snapshots (`rsemu-server`)
- Linux userspace emulation for static and dynamic RISC-V binaries
- Cranelift JIT (`-jit`) for hot guest code
- `riscv-objdump` ELF disassembler

## Building

```shell
$ cargo build --release
```

This builds `target/release/rsemu`, `target/release/rsemu-server` and
`target/release/riscv-objdump`.
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
| `-jit-backend cranelift\|direct` | select the compiler; `direct` is an experimental x86-64 emitter (implies `-jit`) |
| `-jit-threshold n` | executions before a block is compiled (default: 50 to 1000, depending on the compiler backlog; implies `-jit`) |
| `-jit-threads n` | background compiler threads (default: half the host's hardware threads, at most 4; implies `-jit`) |
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

### Checkpoints

`-control path` makes the emulator take commands on the Unix socket
`path`, one per line, each answered with `OK [details]` or
`ERR message`:

- `checkpoint FILE` stops the CPU, saves the system to the snapshot
  `FILE` like `-suspend` does, and leaves the CPU stopped, so that the
  process holds the system in exactly the saved state. The devices
  keep running.
- `resume` continues the stopped CPU.
- `quit` stops the system like a power-off, so that the emulator saves
  its `-jit-cache` and exits.

## Sockets (vsock)

`-device virtio-vsock-device,cid=3,uds=PATH` adds a VirtIO socket
device. Host programs reach the guest's `AF_VSOCK` sockets through the
Unix socket `PATH`, in the style of Firecracker: a host program
connects to it and writes `CONNECT <port>\n`; the emulator answers
`OK <host port>\n` when a guest program accepts the connection on
that port, or closes the socket if none listens, and the socket then
carries the stream. A guest connection to port P of the host (CID 2)
is forwarded to the Unix socket `PATH_P`.

Connections end when the system is suspended. A restored system
sends the guest a transport reset event, which resets the guest's
connected sockets; listening sockets stay. Restoring with
`-vsock-uds PATH` makes the device listen on another socket than the
one of the suspended system, so that several VMs restored from one
snapshot do not collide.

## Shell API with continuations

`rsemu-server` runs shell commands in VMs over HTTP and streams their
output back. Each command ends with a snapshot of its VM, and its id,
the continuation, restores that VM for the next command, which runs in
the same shell:

```shell
$ make -C /path/to/riscv-rs/guest/vmagent
$ /path/to/riscv-rs/guest/rootfs/mkimage.sh
$ ln -s /path/to/riscv-rs/guest/vmagent riscv/cmd/goemu/
$ ln -s /path/to/riscv-rs/guest/rootfs/build riscv/cmd/goemu/rootfs
$ cd riscv/cmd/goemu
$ /path/to/rsemu-server -config /path/to/riscv-rs/configs/vmagent.goemu \
      -warmup "$(cat /path/to/riscv-rs/configs/vmagent-warmup.sh)"
rsemu-server: booting the base VM
rsemu-server: warmup in 7.9s
rsemu-server: base snapshot rsemu-server/snapshots/base.snap in 8.8s
rsemu-server: listening on http://127.0.0.1:8080

$ curl -N localhost:8080/v1/exec -d '{"command": "cd /tmp; X=42; node --version; ls nope"}'
{"data":"v26.11.1","type":"stdout"}
{"data":"\n","type":"stdout"}
{"data":"ls: ","type":"stderr"}
{"data":"cannot access 'nope': No such file or directory\n","type":"stderr"}
{"code":2,"type":"exit"}
{"id":"c8b081182ef1fb32290a9df206fd2271","type":"continuation"}

$ curl -N localhost:8080/v1/exec -d '{"command": "echo $X in $(pwd)",
                                     "continuation": "c8b081182ef1fb32290a9df206fd2271"}'
{"data":"42 in /tmp\n","type":"stdout"}
{"code":0,"type":"exit"}
{"id":"8c36e3efcae3ac3f7fd35ea497c10243","type":"continuation"}
```

- `POST /v1/exec` takes `command`, an optional `continuation` and an
  optional `timeout` in seconds (default `-timeout`, 600). Without a
  continuation the command runs in a fresh VM restored from the base
  snapshot, which the server boots once and keeps in `-state`. The
  response is `application/x-ndjson`, streamed as the command runs:
  `stdout` and `stderr` events with the output in `data`, or in
  `data_base64` if it is not UTF-8; an `exit` event with the status;
  and a `continuation` event with the new id once the snapshot is
  saved. If the command times out, the client disconnects or the VM
  stops, the stream ends with an `error` event (or just ends) and no
  continuation is made.
- An unknown continuation id is an error: `404` with
  `{"error": "unknown continuation id: ..."}`.
- Continuations are immutable: continuing one makes a new one, and the
  same one can be continued any number of times, also concurrently,
  each time from the same state. `DELETE /v1/continuations/<id>`
  removes one.
- Each VM is its own `rsemu` process, started with `-control`. After
  a command, the server checkpoints the VM to the new snapshot file
  and keeps it warm: paused, with its RAM in memory, for `-warm-ttl`
  seconds (default 60). A request for that continuation resumes the
  warm VM (2-5 ms) instead of restoring the snapshot in a new process
  (about 12 ms; the RAM is mapped copy-on-write). At most `-warm` VMs
  (default 4) are kept; the one idle the longest is stopped to make
  room. A paused VM does not run, so its guest clock stands still and
  it continues exactly from the snapshot. The snapshot stays, so a
  continuation whose VM is busy, evicted or expired is restored from
  it, and two requests for one continuation get one warm VM and one
  restored VM. `-warm 0` turns this off. With warm VMs, a command
  takes about 65 ms end to end; with a restore, about 125 ms; most of
  either is writing the snapshot. `GET /v1/health` reports the number
  of warm VMs.

### The default image

[configs/vmagent.goemu](configs/vmagent.goemu) boots a Debian trixie
(slim) disk with Node.js 26, npm and Python 3.13, and 2 GiB of RAM.
[guest/rootfs/mkimage.sh](guest/rootfs/mkimage.sh) makes it,
`node.ext4`, from the riscv64 image of `debian:trixie-slim` on Docker
Hub, a Node.js build for riscv64, CPython from python-build-standalone
and Debian's libatomic, which it needs, all pinned by digest; it needs
fakeroot and no root. Node.js publishes no official riscv64 builds,
and its unofficial builds stopped at v26.0.0, so the image uses the
builds Cloud-V makes natively on RISC-V hardware. The disk has no
network access, so commands cannot install from the npm registry:
copy dependencies into the guest (for example with a `node_modules`
tarball, base64-encoded in the command) or add them to the image.
[configs/vmagent-buildroot.goemu](configs/vmagent-buildroot.goemu)
uses the buildroot disk instead.

Both configurations set `"novector": true` (`-novector`): the device
tree then does not advertise the V extension, of which the emulator
implements a few instructions only. Software that finds V uses it;
OpenSSL in Node.js, for example, crashes npm with an illegal
instruction otherwise.

`-warmup` runs a command in the base VM before its snapshot. With the
JIT, the server gives all VMs the JIT code cache `jit.cache` in the
state directory, and the base VM saves the code it compiled there
before it exits, so that the code the warmup ran starts compiled in
every VM. The base VM runs with `-jit-opt`: its code compiles about
twice as slowly but runs faster in all the other VMs. [configs/vmagent-warmup.sh](configs/vmagent-warmup.sh) runs
node, its test runner, npm and python. Node.js is slow in the emulator
all the same: a bare `npm --version` executes 500 million guest
instructions. Measured through the API (medians of 3, 4-core host):

| Command | Time |
|---------|-----:|
| `node -e '...'` | 0.7 s |
| `npm --version` | 2.1 s |
| `node --test` with 3 small tests (crypto, an HTTP server on localhost, fetch) | 3.2 s |
| `python3 -c 'print(1)'` | 0.5 s |
| Python: a 3M-iteration loop, SHA-256 of 10 MB, JSON of 100k objects | 5.1 s |

The guest clock advances with the instructions executed (one tick of
the 100 MHz timebase per instruction), not with the host clock, so
times measured in the guest, such as Python's `time.perf_counter()`,
are not wall-clock times: a JIT-compiled loop runs more than 100
million instructions per second and the guest clock runs fast.

The guest side is [guest/vmagent](guest/vmagent), a static program
started as `/init` of an initramfs (`vmagent.cpio`). It makes the root
an overlay of a tmpfs on the read-only disk, so all file changes are
in RAM and in the snapshot. Then it adds itself to `/etc/inittab` and
starts the disk's init (a busybox init), or, with `vmagent.init=none`
on the kernel command line as for the Debian disk, which has no init,
it stays PID 1: it mounts `/proc`, `/sys`, `/dev/pts`, `/dev/shm` and
`/run`, brings up the loopback interface, starts the agent and reaps
orphaned processes. Started by init, it listens
on vsock port 1024 and keeps one `/bin/sh` running. A command is
written to a file and sourced in that shell with stdin from
`/dev/null`, so its working directory, variables, functions and
background jobs are there for the next command. A script with a
syntax error is rejected (`sh -n`) without running; a command that
ends the shell (`exit 3`) reports its status, and the next command
gets a new shell. The guest clock continues from the time the snapshot
was taken.

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

With `-jit`, hot guest code is compiled to native code with Cranelift
on background threads (`-jit-threads`) while the interpreter keeps
running. Code becomes hot when the interpreter has reached it 50 times
plus 4 for each block waiting for the compiler, at most 1000 times, so
that the compiler takes lukewarm code when it is idle and only the
hottest code when it has a backlog; `-jit-threshold` fixes the count.
Compiling costs about 15 µs per guest instruction, mostly in
Cranelift's register allocator, so a large program such as Node.js
keeps the compiler threads busy for its first seconds:

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

### Experimental direct x86-64 backend

`-jit-backend direct` selects a handwritten RV64-to-x86-64 compiler;
Cranelift remains the default. The same option is available on
`rsemu-server`. It uses the existing decoder, background compilation,
TLB, guarded block chaining, code invalidation and persistent cache.
Use separate cache files (or server state directories) for the two
backends: their ABIs are different, and a cache from the other backend
is ignored and replaced, not executed.

The emitter assigns six guest registers to fixed host registers using
operand-use counts, weighting instructions inside internal backward-edge
ranges 32 times higher than straight-line/exit instructions. This static
loop heuristic keeps hot counters ahead of cold exit temporaries. Writes
stay in registers until exits or helper calls. Conditional branches fall
through to the next emitted successor when possible, with side exits and
guarded backedges emitted out of line; loop-limit checks are preserved.
Memory fast paths keep the TLB base in callee-saved RBP, fold address
additions into indexed x86 memory operands, and fall through on hits.
Helper paths are out of line. A typical standalone integer load uses
13 emitted host instructions instead of 17, and a store 14 instead of
18, including the instruction-count update but excluding block entry.
Interpreter-observed stable loads can predict a fixed TLB slot, reducing
the predicted-hit load path to 10 instructions. This is a guarded hint:
the actual address must match the live read tag, and a prediction miss
tries the generic inline TLB lookup before calling a helper.
Straight-line reads of an unchanged base can share one checked RAM-page
translation, including across nonfaulting integer arithmetic. Stores,
helpers, base writes and branch entries end a group. A failed range guard
returns before the first read, preserving individual read/fault order in
the interpreter. MMIO and cross-page groups do not use this fast path.
Single-entry read-only loops whose reads all use one invariant base can
also hoist that guard and translation above their backedge. Conditional
branches and side exits are allowed, but stores, helpers, CSRs, fences and
external entries into the middle reject hoisting. The translated base
stays in RDI without reducing the six cached guest registers. Actual
reads still occur on every iteration. Re-entry and the existing 4096-guest-
instruction timeslice limit run the guard again; a guard miss interprets
from the header instead of raising a speculative fault.
It emits integer arithmetic, branches, memory accesses and FP loads,
stores and moves directly; other supported instructions call the same
interpreter/soft-float helpers as the Cranelift backend. No Cranelift
IR, optimization or register allocation is used for direct guest code.
The build still includes Cranelift for the baseline and helper glue.
`-jit-opt` selects multi-block regions for direct too, without a separate
optimizing compiler pass.

Peepholes combine `lui/auipc + addi/addiw` into constants or PC-relative
addressing, `slli + add` and `addi + slli` into scaled x86 `lea`, and
matching shift pairs into sign/zero extension. The intermediate result
must be overwritten, and the second instruction must not be a branch
target. Memory operations are not folded across fault boundaries:
`addi a0,a0,8; ld a0,0(a0)`, for example, must expose the updated a0 if
the load faults. Guest instruction counts still include both fused
instructions; JIT statistics report the number of compiled fused pairs.

Reproduce the Linux/Node comparison (run from this repository):

```shell
cargo build --release --no-default-features --features jit
python3 scripts/jit-bench.py --workdir /path/to/riscv/cmd/goemu \
    --config configs/vmagent.goemu --repetitions 3 --repeats 1 \
    --warmup '' --output jit-bench.json
```

The harness alternates backend order, starts each trial with a fresh
cache, checks all Node outputs against independent expected results,
and reports raw measurements and medians. Startup is Linux/agent
readiness plus base snapshot/cache saving, not just kernel init.
Command totals include VM restore and snapshot overhead. The integer
loop also reports host wall time between streamed markers around a
300-million-iteration loop after a 1-million-iteration warmup, to separate
hot execution from Node startup. Guest clocks are not used for timing.

An initial x86-64 orb experiment with Node 26.11.1 gave these medians
of three trials, using the command above and the default (non-`-jit-opt`)
Cranelift backend as the baseline:

| Workload | Cranelift | Direct |
|----------|----------:|-------:|
| Linux/agent startup, empty JIT cache | 1.668 s | 0.759 s |
| Fresh Node startup, including restore/snapshot | 2.591 s | 2.420 s |
| Fresh Node crypto/JSON, including restore/snapshot | 3.758 s | 3.226 s |
| Node integer loop body, fresh VM restore | 0.421 s | 0.511 s |
| Node integer loop body, continued VM restore | 0.438 s | 0.528 s |

This demonstrates a startup/end-to-end benefit, **not** a general win in
generated-code execution speed: the isolated loop is about 22% slower.
Host OS caches were not flushed; the host is shared, and three trials
are exploratory rather than a statistical performance guarantee.
Continued runs start a new Node process in the previous VM state.

A follow-up measured the two loop optimizations separately, first after
each change and then with all variants interleaved. To reduce marker
overhead, each hot window called the same 300-million-iteration function
ten times after the same warmup. Medians of three interleaved trials:

| Backend/step | Hot window, fresh restore | Hot window, continued restore |
|--------------|--------------------------:|------------------------------:|
| Direct, before loop optimizations | 5.067 s | 5.255 s |
| Direct, loop-weighted register allocation | 4.658 s | 4.845 s |
| Direct, plus branch fallthrough/layout | 4.502 s | 4.535 s |
| Cranelift control | 4.080 s | 4.361 s |

The combined improvement was about 11% fresh and 14% continued, but
direct still did not beat Cranelift. Individual trials were noisy on
the shared host; these are exploratory measurements, not a guarantee.
Each variant used isolated state and an empty initial JIT cache, and
all Node outputs were checked against the loop's closed-form sum.

A separate memory-path experiment compared four versus six cached
guest registers and the revised memory scheduling/layout, with three
interleaved trials per variant. Fresh-restore hot-window medians:

| Variant | Integer loop, 10 × 300M iterations | Typed-array reads, 3 × 100M iterations |
|---------|----------------------------------:|--------------------------------------:|
| Previous memory path, six cached registers | 4.393 s | 1.289 s |
| Previous memory path, four cached registers | 5.168 s | 1.363 s |
| Revised memory path, six cached registers | 4.356 s | 1.122 s |
| Revised memory path, four cached registers | 4.985 s | 1.321 s |

Six cached registers were retained. The revised path improved typed-array
reads by 13% fresh and 6% continued (1.253 s to 1.183 s). The integer
loop was essentially unchanged fresh and 10% faster continued (4.777 s
to 4.321 s). Timings remain subject to shared-host noise; hardware
stall counters were unavailable, so these are not direct stall measurements.
The typed-array workload cyclically sums a 16384-element `Uint32Array`
initialized with `(i * 17) ^ 0x12345`; all results were independently checked.

Further TLB experiments used three rotating-order trials per variant,
with isolated caches/state, Linux startup, and fresh/continued Node
processes. Smaller split read/write tables lost badly on address conflicts:
eight pages spaced 1 MiB apart took 0.554 s with 256 entries and 0.297 s
with 1024, versus 0.128 s originally. The 4096-entry split layout took
0.133 s and did not improve the small-array test either; it was discarded.
Split entries were 16-byte `{tag, delta}` records, with separate read/write
halves per privilege bank; the interpreter's ordinary TLB stayed at 4096.

Keeping the original table and dedicating RBP to its base produced mixed
results, not an overall win. Adding translation reuse improved the
four-field object-read test versus RBP alone by 6% fresh / 13% continued.
The final comparison against the original memory path was also mixed:

| Workload | Original fresh / continued | RBP + reuse fresh / continued |
|----------|---------------------------:|------------------------------:|
| Linux/agent readiness (startup only) | 0.713 s | 0.811 s |
| Node startup (whole request) | 1.733 / 1.759 s | 1.784 / 1.629 s |
| Integer loop, 300M iterations (hot) | 0.414 / 0.414 s | 0.435 / 0.428 s |
| Crypto/JSON (whole request) | 3.030 / 2.473 s | 2.792 / 2.322 s |
| Small-array reads, 30M (hot) | 0.125 / 0.126 s | 0.119 / 0.114 s |
| Conflict reads, 30M (hot) | 0.137 / 0.139 s | 0.133 / 0.135 s |
| 2048-page reads, 30M (hot) | 0.261 / 0.258 s | 0.247 / 0.264 s |
| Array read/modify/write, 30M (hot) | 0.176 / 0.175 s | 0.168 / 0.165 s |
| Four-field object reads, 30M (hot) | 0.349 / 0.353 s | 0.331 / 0.335 s |

The combined version remains experimental: lower memory-path instruction
counts did **not** improve the original integer loop or Linux startup.
These are shared-host wall times, not hardware-counter measurements.
Readiness includes Linux, agent and base snapshot; whole requests include
restore/snapshot overhead. Hot times are streamed-marker intervals after
1M warmup iterations. All 444 Node executions across 30 VM launches
checked exact output against independent oracles. Raw per-trial results
and JavaScript commands are in `.amp/in/artifacts/tlb-{size,register,reuse,final}-experiment.json`.

### Profile-guided fixed-slot read experiment

The direct backend records successful aligned integer RAM reads during
interpretation, without extra memory accesses. A bounded 16,384-entry
profile table keys observations by physical instruction address and raw
instruction. After eight observations of the same TLB slot, compilation
can embed that slot's displacement instead of computing the index on
every read. Sites observed using different slots remain generic. The
adaptive direct hot threshold has a floor of nine to permit training;
explicit thresholds and Cranelift are unchanged.

Generated code still checks the actual page/alignment tag and reads the
current translation delta. It embeds neither host pointers nor permission
assumptions. Address changes, slot collisions and invalidations therefore
retain the normal TLB checks and fault behavior. Stores are unchanged.
Grouped reads retain their generic range guard: predicting only one slot
would not prove that both ends of the group's range share a page.

Three interleaved baseline/specialized trials with longer timing windows
gave these median hot times (baseline is the preceding RBP + reuse path):

| Node workload | Baseline fresh / continued | Fixed-slot fresh / continued |
|---------------|---------------------------:|-----------------------------:|
| Integer loop, 10 × 300M | 4.206 / 4.057 s | 4.171 / 3.856 s |
| Array reads, 3 × 100M | 1.065 / 1.120 s | 1.111 / 1.088 s |
| Four-field reads, 3 × 100M | 3.222 / 3.095 s | 3.156 / 3.054 s |

This is a modest, mixed result, not a decisive overall speedup: integer
reads improved about 1% fresh / 5% continued, array reads regressed 4%
fresh / improved 3% continued, and field reads improved 2% / 1%.
These are shared-host wall times without hardware counters; the larger
gains in the short-window sweep did not hold consistently. Linux/agent
readiness in the longer batch was 0.710 → 0.657 s, but startup is noisy.
The final short and long cohorts used 15 fresh boot/cache trials and
checked exact output for 180 Node commands. Raw trials, workload commands
and the profile-only control are in
`.amp/in/artifacts/load-slot-experiment.json` and
`.amp/in/artifacts/load-slot-long-experiment.json`.

A separate diagnostic warm run recorded 16,214,420 interpreter training
samples, 806 stable and 740 polymorphic resident sites, and 27,712 emitted
fixed-slot lookups. These are not dynamic specialization hit rates or
whole-program unique-site counts; loaded cached code is not counted as
new emission. See `.amp/in/artifacts/load-profile-after.txt`.

### Loop-invariant translation experiment

The first version accepted only straight-line loops. It improved a
two-load invariant-base microbenchmark but emitted no hoisted loops in
the Linux/Node diagnostic run. Allowing conditional branches and side
exits, while still requiring the entry guard to dominate every read,
made the pass apply to Node. Diagnostic runs emitted five hoisted loops
for Node startup and seven for the integer workload; these are static
emission counts, not runtime hit counts. Linux boot emitted none.

Three interleaved baseline/hoisted trials used fresh, isolated caches and
the preceding fixed-slot direct backend as the baseline. Hot workloads
use the same longer timing windows as the fixed-slot experiment:

| Workload | Baseline fresh / continued | Hoisted fresh / continued |
|----------|---------------------------:|--------------------------:|
| Linux/agent readiness (startup only) | 0.715 s | 0.707 s |
| Node startup (whole request) | 1.783 / 1.663 s | 1.744 / 1.827 s |
| Crypto/JSON (whole request) | 3.287 / 2.460 s | 3.648 / 2.354 s |
| Integer loop, 10 × 300M (hot) | 3.801 / 3.886 s | 3.665 / 3.637 s |
| Array reads, 3 × 100M (hot) | 1.053 / 1.047 s | 1.039 / 1.063 s |
| Four-field reads, 3 × 100M (hot) | 3.237 / 3.062 s | 3.162 / 3.344 s |

The integer workload improved about 4% fresh / 6% continued. The other
workloads remain mixed, including slower continued Node startup and
field reads: this is not an overall speedup. Shared-host wall times and
three samples per case do not establish hardware-level causality.
All 60 final Node commands checked exact output across six fresh
boot/cache trials. `.amp/in/artifacts/loop-hoist-experiment.json` contains
the commands, individual timings and binary hashes; the rejected
straight-line-only version is in `loop-hoist-straight-experiment.json`.

A separate user-mode RV64 microbenchmark performed 600M iterations with
two invariant reads per iteration and checked the final sum independently.
Median whole-process time improved 0.966 → 0.758 s (22%) over three
interleaved trials. This measures the intended case, not Linux or Node.
Its source and raw results are in
`.amp/in/artifacts/loop-hoist-microbenchmark.json`. Static diagnostic
summaries are in `.amp/in/artifacts/loop-hoist-diagnostics.txt`; temporary
checkpoint logging was removed before the timed runs.

### Host-mapped RAM experiment

`-jit-backend direct-mmap` is a Linux/x86-64 opt-in experiment, not a
replacement for `direct`. Four sparse 512 GiB host windows separate the
existing privilege/SUM banks. Guest RAM pages are mapped with effective
read/write permissions into these windows from coherent shared memfd RAM.
Standalone JIT accesses check Sv39 canonicality and alignment, establish an out-of-line recovery
label, then access `[RBP + guest_address]` without a software TLB
lookup. Unmapped aliases fault back to the normal memory helper; the
SIGSEGV handler only redirects execution, never walks tables or maps pages.
SFENCE, SATP changes and permission revocation invalidate the aliases.
MMIO, noncanonical and unaligned addresses retain ordinary slow paths.
Writes use host aliases only for guest-writable, dirty RAM pages without
compiled code or an overlapping store overlay. A first write to a clean
guest page faults through the ordinary helper, which sets the guest dirty
bit before enabling host writes. Successful inline stores clear the LR/SC
reservation. Compiling a physical page revokes writes from every host
alias, including aliases evicted from the software TLB; later stores fault
through the code-invalidation helper. This applies to synchronous and
background compilation and cached-code validation. When an overlay is
installed, generated stores retain the ordinary helper path. Existing
grouped and loop-hoisted reads retain their checked RAM translations;
they do not use the host alias path.

This is **not a single-instruction memory-access implementation**: address
guards and recovery setup remain. It reserves 2 TiB of virtual address
space per CPU, maps pages individually, and is subject to host VMA limits.
Snapshots copy sparse source extents into the memfd and save allocated
memfd extents, rather than relying on private-COW pagemap bits. Scanning
unallocated shared RAM through mmap would instantiate zero pages. Cache
fingerprints distinguish this ABI from both other backends.

Three interleaved isolated-cache trials compared the committed direct
backend with the initial read-only alias experiment. All 60 Node commands
checked exact stdout, including fresh restores and continuation restores. Medians:

| Workload | Direct fresh / continued | Mmap fresh / continued |
|----------|-------------------------:|-----------------------:|
| Linux/agent readiness (startup only) | 0.811 s | 1.017 s |
| Node startup (whole request) | 2.473 / 1.813 s | 1.808 / 1.387 s |
| Crypto/JSON (whole request) | 3.508 / 2.686 s | 2.494 / 2.182 s |
| Integer loop, 10 × 300M (hot) | 3.667 / 3.652 s | 3.641 / 3.642 s |
| Array reads, 3 × 100M (hot) | 1.056 / 1.171 s | 1.196 / 1.205 s |
| Four-field reads, 3 × 100M (hot) | 3.132 / 3.182 s | 3.085 / 3.142 s |

There is no convincing hot-loop improvement: integer and field times are
approximately unchanged, and array reads regress. Whole-request gains
must not be attributed to faster guest memory access: they include the
different RAM/snapshot implementation. For example, fresh Node startup
time through the command's exit event worsened 1.378 → 1.631 s even though
whole-request time improved. Linux readiness also regressed. Shared-host
wall times and three samples per case do not establish hardware-level
causality. Raw commands, binary hashes, exit times and loop-body times are
in `.amp/in/artifacts/host-mmap-experiment.json`.

The write extension was compared against the read-only alias binary in
three new interleaved trials per variant, with identical shared RAM and
snapshot handling. All 72 Node commands checked exact output. The added
write workload overwrites a 4096-element Int32Array for 3 × 100M iterations
and checks the final sum against an independently computed result.

| Workload | Read-only fresh / continued | Read/write fresh / continued |
|----------|----------------------------:|-----------------------------:|
| Linux/agent readiness (startup only) | 0.963 s | 1.223 s |
| Node startup (whole request) | 2.141 / 1.724 s | 1.770 / 1.421 s |
| Crypto/JSON (whole request) | 3.016 / 2.268 s | 3.045 / 2.387 s |
| Integer loop (hot) | 3.817 / 3.973 s | 3.815 / 3.725 s |
| Array reads (hot) | 1.449 / 1.174 s | 1.238 / 1.168 s |
| Field reads (hot) | 3.689 / 3.435 s | 3.620 / 3.255 s |
| Array writes (hot) | 1.344 / 1.345 s | 1.227 / 1.278 s |

Array writes improve about 9% fresh / 5% continued; Node startup improves
about 17–18%, but Linux readiness regresses about 27% and crypto is mixed.
These remain noisy, three-sample shared-host measurements, not a general
speedup claim. Raw commands, timings, binary hashes and the reproduction
script are in `.amp/in/artifacts/host-mmap-writes.json`.

### Mmap alignment-guard removal

The read/write mmap checkpoint is preserved on `experiment/host-mmap`;
the guard-removal follow-up is on `experiment/mmap-guards`. The ordinary
`direct` backend is unchanged: its alignment check is already folded into
the software-TLB tag comparison, not a separate branch.

For mmap accesses, a forward analysis tracks the possible low three bits
of integer registers through constants, masks, shifts and arithmetic. It
includes signed memory offsets and runs on decoded guest instructions,
independent of peephole fusion. Facts reset at branch entries, trace gaps
and helpers. A load invalidates facts for its integer destination. When
all possible effective addresses satisfy the access's alignment, the
emitter omits the three-instruction alignment guard. Unknown addresses
keep the guard; canonical-address checks and fault recovery remain.
No observed pointer value or guest ABI promise is treated as a proof.

An untimed diagnostic emitted 1,391 fewer alignment guards during Linux
boot, 1,766 during fresh Node startup and 2,086 during the integer workload.
These count newly emitted access sites, not runtime executions; restored
cached code is not counted again. See
`.amp/in/artifacts/mmap-alignment-diagnostics.txt`. Temporary checkpoint
logging was removed; the timed binaries did not contain it.

Three rotated-order trials per variant used isolated caches and checked
all 108 Node commands' exact output. Medians below show fresh / continued
runs. Request-through-exit includes restore and command execution, but
excludes saving the resulting snapshot; hot times cover only loop bodies.

| Workload | Direct | Guarded mmap | Guard-elided mmap |
|----------|-------:|-------------:|-----------------:|
| Linux/agent readiness | 0.710 s | 1.113 s | 1.182 s |
| Node request through exit | 1.170 / 0.986 s | 1.579 / 1.112 s | 1.581 / 1.191 s |
| Crypto/JSON through exit | 2.254 / 1.747 s | 2.580 / 2.207 s | 2.350 / 1.999 s |
| Integer loop (hot) | 3.886 / 3.688 s | 3.797 / 3.778 s | 3.662 / 3.715 s |
| Array reads (hot) | 1.447 / 1.161 s | 1.222 / 1.149 s | 1.153 / 1.163 s |
| Field reads (hot) | 3.602 / 3.084 s | 3.166 / 3.208 s | 3.230 / 3.080 s |
| Array writes (hot) | 1.234 / 1.245 s | 1.287 / 1.285 s | 1.307 / 1.336 s |

The pass removes guards in real workloads, but does not establish an
overall win: some loop medians improve, while array writes, Linux
readiness and Node time through exit do not. Shared-host variability is
substantial, with only three samples per case. The evidence still does
not justify replacing ordinary direct mode with mmap. Raw commands,
timings, hashes and the reproduction script are in
`.amp/in/artifacts/mmap-alignment-guards.json`.

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
