#!/bin/sh
# Makes ctest.ext2, a disk with crun and the container test, to attach
# as a second virtio disk: rsemu ... CONFIG ctest.ext2, then in the
# guest: mount /dev/vdb /mnt && sh /mnt/ctest.sh
set -eu
CRUN=1.30.1
SHA256=2fb25ecb01bb43e3166b5370b33f7b71f241583d646288a99a4fb296cfefc1e4

here=$(cd "$(dirname "$0")" && pwd)
tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT
curl -fLo "$tmp/crun" \
	https://github.com/containers/crun/releases/download/$CRUN/crun-$CRUN-linux-riscv64-disable-systemd
echo "$SHA256  $tmp/crun" | sha256sum -c -
mkdir -p "$tmp/disk"
install -m 755 "$tmp/crun" "$here/ctest.sh" "$tmp/disk/"
for b in bundle bundle-userns; do
	mkdir -p "$tmp/disk/$b/rootfs"
	cp "$here/$b/config.json" "$tmp/disk/$b/"
done
rm -f ctest.ext2
mke2fs -q -t ext2 -d "$tmp/disk" ctest.ext2 256M
echo ctest.ext2
