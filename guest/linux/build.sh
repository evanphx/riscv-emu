#!/bin/sh
# Builds the rsemu guest kernel: Linux $VER, riscv defconfig plus
# rsemu.config, with the patches in patches/.
#
# usage: build.sh [OUTDIR]   (default: ./build)
# Needs a riscv64 cross compiler (CROSS_COMPILE, default
# riscv64-linux-gnu-), curl, flex, bison, bc and the usual kernel
# build tools. The kernel is OUTDIR/obj/arch/riscv/boot/Image.
set -eu

VER=7.0.11
SHA256=e56c8356dda01136a6041c6ef832bd0ec99bd2d35dff97832aa5ec10ed014304

here=$(cd "$(dirname "$0")" && pwd)
out=$(mkdir -p "${1:-$here/build}" && cd "${1:-$here/build}" && pwd)
make="make -C $out/linux-$VER ARCH=riscv CROSS_COMPILE=${CROSS_COMPILE:-riscv64-linux-gnu-} O=$out/obj"

cd "$out"
if [ ! -d linux-$VER ]; then
	[ -f linux-$VER.tar.xz ] ||
		curl -fLO https://cdn.kernel.org/pub/linux/kernel/v7.x/linux-$VER.tar.xz
	echo "$SHA256  linux-$VER.tar.xz" | sha256sum -c -
	tar xf linux-$VER.tar.xz
	for p in "$here"/patches/*.patch; do
		patch -d linux-$VER -p1 <"$p"
	done
fi

# Merge the fragment into the minimal defconfig, not the expanded one,
# so that options defconfig only gets by select are recomputed.
mkdir -p obj
cp linux-$VER/arch/riscv/configs/defconfig obj/.config.base
(cd obj && "$out/linux-$VER/scripts/kconfig/merge_config.sh" -m -O . \
	.config.base "$here/rsemu.config" >/dev/null)
$make -s olddefconfig

# Check that every option of the fragment survived the dependencies.
sed -n 's/^\(CONFIG_[A-Z0-9_]*\)=\(.*\)/\1=\2/p; s/^# \(CONFIG_[A-Z0-9_]*\) is not set/\1=n/p' \
	"$here/rsemu.config" | while IFS== read -r opt want; do
	got=$(sed -n "s/^$opt=//p" obj/.config)
	if [ "${got:-n}" != "$want" ]; then
		echo "$opt: want $want, got ${got:-n}" >&2
		exit 1
	fi
done

$make -j"$(nproc)" Image
echo "$out/obj/arch/riscv/boot/Image"
