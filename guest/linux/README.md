# Guest Linux kernel

A Linux 7.0.11 kernel for rsemu: quick to boot, able to run
containers, and with drivers only for the emulator's devices.
`build.sh` downloads the source, applies `patches/`, merges
`rsemu.config` into the riscv defconfig and builds `Image`:

    guest/linux/build.sh [OUTDIR]

`configs/buildroot-rsemu-kernel.goemu` boots buildroot with it; copy
`OUTDIR/obj/arch/riscv/boot/Image` and `OUTDIR/obj/System.map` to
`linux-7.0.11-rsemu/` next to the other guest images.

## Configuration

`rsemu.config` changes defconfig in three ways:

- Startup. Tracing is off: defconfig spends about a billion
  instructions before init rewriting trace event formats (see
  below). So are the NFS client, legacy BSD ptys (0.35 s of guest time),
  the unaligned access probe (the emulator's unaligned accesses are
  fast; this makes the kernel nonportable), debug options, kexec,
  suspend, cpufreq and cpuidle, EFI and ACPI.
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

There are no modules: everything is built in. Some options are cheap
as modules but slow when built in, so they are off: the crypto DRBG
selects jitterentropy, whose self-test takes 1.3 s at boot (and
XFRM/ESP needs the DRBG), and btrfs benchmarks the raid6 and xor
routines (0.65 s).

## Patches

`0001-tracing-eval-map-lookup.patch` makes the update of trace event
formats with enum values (trace_event_update_all) look up the names
in a hash, rather than scanning each format once per enum map. With
tracing on, it takes boot to init from 13.07 s to 1.16 s of guest
time, and the formats in tracefs are byte-identical. It does nothing
with the configuration here, which has tracing off.

## Timings

Seconds, medians of 3 interleaved runs of rsemu with the buildroot
image (`init`: the kernel starts init; `session`: login, workloads and
poweroff):

| mode         | kernel            | init | login | session |
|--------------|-------------------|-----:|------:|--------:|
| interpreter  | defconfig         | 9.95 | 11.04 |   32.62 |
| interpreter  | defconfig + patch | 2.02 |  3.11 |   24.70 |
| interpreter  | rsemu.config      | 0.81 |  1.88 |   23.23 |
| -jit         | defconfig         | 1.76 |  2.51 |    8.12 |
| -jit         | rsemu.config      | 0.49 |  1.23 |    6.86 |
| -jit-opt     | defconfig         | 0.84 |  1.22 |    5.79 |
| -jit-opt     | rsemu.config      | 0.19 |  0.55 |    5.09 |

## Container test

`ctest/mkdisk.sh` makes `ctest.ext2`, a disk with crun and two OCI
bundles. Attach it as a second disk and run the test in the guest:

    rsemu -jit configs/buildroot-rsemu-kernel.goemu ctest.ext2
    # mount /dev/vdb /mnt && sh /mnt/ctest.sh

It runs a container with and without a user namespace, on an overlay
of a copy of the guest's root, and checks the cgroup limits (memory,
pids, cpu), a seccomp rule, the device rules, and that veth, bridge
and netfilter are available.
