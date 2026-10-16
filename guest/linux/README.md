# Guest Linux kernel

A Linux 7.0.11 kernel for rsemu: quick to boot, able to run
containers and portable (CO-RE) eBPF programs, and with drivers only
for the emulator's devices. `build.sh` downloads the source, applies
`patches/`, merges `rsemu.config` into the riscv defconfig and builds
`Image`:

    guest/linux/build.sh [OUTDIR]

It needs gcc >= 14 for riscv64 (the function tracer needs
`-fmin-function-alignment`); without `CROSS_COMPILE` it downloads
kernel.org's gcc 15.2. BTF needs pahole >= 1.22 (dwarves).

`configs/buildroot-rsemu-kernel.goemu` boots buildroot with it from
`linux-7.0.11-rsemu/` next to the other guest images. A build of this
directory's sources is in [linux-7.0.11-rsemu](linux-7.0.11-rsemu);
copy or link it there, or copy `OUTDIR/obj/arch/riscv/boot/Image` and
`OUTDIR/obj/System.map` from your own build:

    ln -s /path/to/riscv-rs/guest/linux/linux-7.0.11-rsemu riscv/cmd/goemu/

## Configuration

`rsemu.config` changes defconfig in four ways:

- Startup. Off: the NFS client, whose trace events cost defconfig
  about a billion instructions before init (see the patches), legacy
  BSD ptys (0.35 s of guest time), the unaligned access probe (the
  emulator's unaligned accesses are fast; this makes the kernel
  nonportable), debug options, kexec, suspend, cpufreq and cpuidle,
  EFI and ACPI.
- Devices. Only what the emulator has: virtio over MMIO (block, net,
  rng, console, balloon, GPU, input), the 16550 UARTs, the PLIC, the
  goldfish RTC and syscon power-off. No PCI, USB, sound, MMC, SCSI,
  SoC platforms or other network drivers.
- Containers. Namespaces (user and time included), cgroup v2 with all
  the controllers runtimes use, cgroup BPF for device control,
  seccomp, overlayfs, FUSE, squashfs, veth, bridge, macvlan, ipvlan,
  vxlan, tun, iptables (legacy and nft), nftables, NAT, conntrack and
  IPVS. Moby's check-config.sh finds everything except encrypted
  overlay networks (XFRM/ESP) and btrfs.
- eBPF. Kernel BTF (`/sys/kernel/btf/vmlinux`) for CO-RE relocations,
  kprobes, tracepoints, the function tracer for fentry/fexit, and the
  JIT, always on.

Everything is built in; `build.sh` fails if an option is left as a
module. Some options are cheap as modules but slow when built in, so
they are off: the crypto DRBG selects jitterentropy, whose self-test
takes 1.3 s at boot (and XFRM/ESP needs the DRBG), and btrfs
benchmarks the raid6 and xor routines (0.65 s). CONFIG_MODULES stays
on even so: on riscv64 the BPF JIT breaks without it. The module/BPF
region is then not preallocated in every page table, so the first JIT
allocation faults in processes created before it, and without
STRICT_MODULE_RWX JIT output is written straight to read-only memory.

## Patches

`0001-tracing-eval-map-lookup.patch` makes the update of trace event
formats with enum values (trace_event_update_all) look up the names
in a hash, rather than scanning each format once per enum map. With
tracing on, it takes boot to init from 13.07 s to 1.16 s of guest
time, and the formats in tracefs are byte-identical.

`0002-riscv-sort-mcount-loc-at-build-time.patch` sorts ftrace's table
of call sites at build time, as x86 and arm64 do, rather than at every
boot (0.23 s of guest time to init in the interpreter). The build
time sorter drops entries outside any function, so it needs to know
that riscv puts 8 bytes of the patchable entry before the function.
FTRACE_SORT_STARTUP_TEST, the function tracer and fentry/fexit
programs work with it.

The others take most of what BTF and tracing still cost at boot, in
instructions before init (165M with 0001 and 0002, 102M with all, 70M
without BTF and tracing):

- `0003` (32M): the kernel's own BTF is part of its read-only data, so
  only its layout is checked when it is parsed, not every name and
  member as for BTF from user space.
- `0004` (17M): ftrace initializes the kernel's 34000 call sites
  through one writable alias of the kernel text, not a fixmap mapping
  and two TLB and icache flushes per write.
- `0005` (3M): the ftrace records of the freed init functions are
  removed with one move rather than one per record.
- `0006` (1.5M): strstr() compares the first character before calling
  strncmp(), and does not measure the string first.
- `0007` (2.3M): syscall metadata is found by binary search, not by
  comparing each syscall's name with all of them.
- `0008` (4M): the riscv64 jump label table is sorted at build time,
  and the kernel only checks that it is sorted.

The formats of all trace events, the syscall events, the function
tracer (also from the command line), kprobes, CO-RE programs and the
container test work the same with them.

## Timings

Seconds, medians of 3 interleaved runs of rsemu with the buildroot
image (`init`: the kernel starts init; `session`: login, workloads and
poweroff). Built with gcc 15.2 unless noted:

| mode         | kernel                                | init | login | session |
|--------------|---------------------------------------|-----:|------:|--------:|
| interpreter  | defconfig (gcc 12)                    | 9.95 | 11.04 |   32.62 |
| interpreter  | rsemu.config without BTF, tracing     | 0.78 |  1.83 |   23.08 |
| interpreter  | ... with BTF                          | 1.03 |  2.07 |   23.52 |
| interpreter  | rsemu.config, patches 0001-0002 only  | 1.99 |  3.09 |   24.70 |
| interpreter  | rsemu.config                          | 1.09 |  2.17 |   24.28 |
| -jit         | defconfig (gcc 12)                    | 1.76 |  2.51 |    8.12 |
| -jit         | rsemu.config without BTF, tracing     | 0.55 |  1.31 |    7.26 |
| -jit         | ... with BTF                          | 0.57 |  1.31 |    7.07 |
| -jit         | rsemu.config, patches 0001-0002 only  | 1.11 |  1.93 |    7.66 |
| -jit         | rsemu.config                          | 0.63 |  1.45 |    7.42 |
| -jit-opt     | defconfig (gcc 12)                    | 0.84 |  1.22 |    5.79 |
| -jit-opt     | rsemu.config without BTF, tracing     | 0.22 |  0.60 |    5.26 |
| -jit-opt     | ... with BTF                          | 0.24 |  0.64 |    5.36 |
| -jit-opt     | rsemu.config, patches 0001-0002 only  | 0.66 |  1.03 |    5.70 |
| -jit-opt     | rsemu.config                          | 0.27 |  0.65 |    5.44 |

With all the patches, BTF and tracing cost 0.3 s of boot in the
interpreter and 0.05-0.1 s with the JITs. Most of what remains is
registering the trace events (their formats are checked and rewritten
with enum values), creating ftrace's records of the call sites, and
reading the BTF types into the kernel's table of them.

## Container test

`ctest/mkdisk.sh` makes `ctest.ext2`, a disk with crun and two OCI
bundles. Attach it as a second disk and run the test in the guest:

    rsemu -jit configs/buildroot-rsemu-kernel.goemu ctest.ext2
    # mount /dev/vdb /mnt && sh /mnt/ctest.sh

It runs a container with and without a user namespace, on an overlay
of a copy of the guest's root, and checks the cgroup limits (memory,
pids, cpu), a seccomp rule, the device rules, and that veth, bridge
and netfilter are available.

## eBPF test

`bpftest/mkdisk.sh VMLINUX` makes `bpftest.ext2` with `bpfcore`, which
loads kprobe, fentry, fexit and tp_btf programs through a libbpf light
skeleton (the kernel applies the CO-RE relocations, so the guest needs
no libbpf), opens some files, and checks what the programs read from
task_struct. VMLINUX is any BTF kernel's vmlinux, for vmlinux.h; using
one whose task_struct differs from the running kernel's exercises the
relocations.

    rsemu -jit configs/buildroot-rsemu-kernel.goemu bpftest.ext2
    # mount /dev/vdb /mnt && /mnt/bpfcore
