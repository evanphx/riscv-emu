#!/bin/sh
# Runs inside the guest. /mnt is this disk.
set -u
echo "== kernel $(uname -r)"
mount -t cgroup2 none /sys/fs/cgroup 2>/dev/null; echo "cgroup2 controllers: $(cat /sys/fs/cgroup/cgroup.controllers)"
mount -t tmpfs tmpfs /tmp 2>/dev/null
[ -d /mnt/lower/bin ] || { mkdir -p /mnt/lower && cd / && tar cf - bin sbin lib lib64 usr etc 2>/dev/null | (cd /mnt/lower && tar xf -); mkdir -p /mnt/lower/proc /mnt/lower/sys /mnt/lower/dev /mnt/lower/tmp; }
for b in bundle bundle-userns; do
  mkdir -p /tmp/$b/up /tmp/$b/work
  mount -t overlay overlay -o lowerdir=/mnt/lower,upperdir=/tmp/$b/up,workdir=/tmp/$b/work /mnt/$b/rootfs && echo "overlay mounted for $b"
  /usr/bin/time /mnt/crun --cgroup-manager=cgroupfs run -b /mnt/$b ct-$b; echo "crun exit=$?"
  echo "host sees written file in upper: $(cat /tmp/$b/up/written 2>&1)"
  umount /mnt/$b/rootfs
done
echo "== host net: veth"; ip link add vh0 type veth peer name vc0 2>&1 && ip link show type veth 2>&1 | grep -c veth
ip link add br0 type bridge 2>&1 && echo "bridge ok"
for f in /proc/net/ip_tables_names /proc/sys/net/netfilter/nf_conntrack_max /proc/sys/net/bridge/bridge-nf-call-iptables; do [ -e $f ] && echo "present: $f" || echo "MISSING: $f"; done
