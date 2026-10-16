#!/bin/sh
# Makes bpftest.ext2, a disk with bpfcore: a CO-RE test that loads
# kprobe, fentry, fexit and tp_btf programs through a libbpf light
# skeleton (the kernel applies the relocations against its own BTF, so
# the guest needs no libbpf), opens some files, and checks what the
# programs read from task_struct. Attach the disk as a second virtio
# disk; in the guest: mount /dev/vdb /mnt && /mnt/bpfcore
#
# usage: mkdisk.sh VMLINUX
# VMLINUX is the vmlinux of any BTF kernel, for vmlinux.h; it need not
# be the kernel the test runs on. Needs clang, bpftool, the libbpf
# headers, riscv64-linux-gnu-gcc with a static glibc and mke2fs.
set -eu
here=$(cd "$(dirname "$0")" && pwd)
vmlinux=$(realpath "$1")
tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT
cd "$tmp"

bpftool btf dump file "$vmlinux" format c >vmlinux.h
clang -g -O2 -target bpf -D__TARGET_ARCH_riscv -I. -Wno-missing-declarations \
	-c "$here/core.bpf.c" -o core.bpf.o
bpftool gen skeleton -L core.bpf.o name core >core.lskel.h

# The skeleton's headers, without the rest of the host's /usr/include.
mkdir -p inc/bpf
for h in skel_internal.h bpf.h libbpf_common.h libbpf_legacy.h libbpf_version.h; do
	cp /usr/include/bpf/$h inc/bpf/
done
mkdir disk
riscv64-linux-gnu-gcc -static -O2 -Iinc -Iinc/bpf -I. -o disk/bpfcore "$here/main.c"

cd - >/dev/null
rm -f bpftest.ext2
mke2fs -q -t ext2 -d "$tmp/disk" bpftest.ext2 32M
echo bpftest.ext2
