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

`configs/buildroot-rsemu-kernel.goemu` boots buildroot with it; copy
`OUTDIR/obj/arch/riscv/boot/Image` and `OUTDIR/obj/System.map` to
`linux-7.0.11-rsemu/` next to the other guest images.

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

## Timings

Seconds, medians of 3 interleaved runs of rsemu with the buildroot
image (`init`: the kernel starts init; `session`: login, workloads and
poweroff). Built with gcc 15.2 unless noted:

| mode         | kernel                              | init | login | session |
|--------------|-------------------------------------|-----:|------:|--------:|
| interpreter  | defconfig (gcc 12)                  | 9.95 | 11.04 |   32.62 |
| interpreter  | rsemu.config without BTF, tracing   | 0.77 |  1.84 |   23.36 |
| interpreter  | ... with BTF                        | 1.03 |  2.12 |   23.68 |
| interpreter  | ... with BTF, tracing, patch 0001   | 2.21 |  3.30 |   25.31 |
| interpreter  | rsemu.config (all patches)          | 1.98 |  3.08 |   24.66 |
| -jit         | defconfig (gcc 12)                  | 1.76 |  2.51 |    8.12 |
| -jit         | rsemu.config without BTF, tracing   | 0.49 |  1.24 |    7.06 |
| -jit         | rsemu.config (all patches)          | 1.05 |  1.88 |    7.56 |
| -jit-opt     | defconfig (gcc 12)                  | 0.84 |  1.22 |    5.79 |
| -jit-opt     | rsemu.config without BTF, tracing   | 0.19 |  0.56 |    5.41 |
| -jit-opt     | rsemu.config (all patches)          | 0.63 |  1.02 |    5.68 |

What remains of the cost of tracing at boot is mostly ftrace turning
its 34000 call sites into nops one at a time, and checking the kernel
BTF (about 25 million instructions, 0.25 s of guest time).

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
