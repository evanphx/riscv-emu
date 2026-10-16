#!/bin/sh
# Builds the rsemu guest kernel: Linux $VER, riscv defconfig plus
# rsemu.config, with the patches in patches/.
#
# usage: build.sh [OUTDIR]   (default: ./build)
#
# Needs curl, flex, bison, bc, pahole >= 1.22 (dwarves) and the usual
# kernel build tools. The cross compiler must be gcc >= 14; without
# CROSS_COMPILE, kernel.org's gcc 15.2 for riscv64 is downloaded. The
# kernel is OUTDIR/obj/arch/riscv/boot/Image.
set -eu

VER=7.0.11
SHA256=e56c8356dda01136a6041c6ef832bd0ec99bd2d35dff97832aa5ec10ed014304
GCC=15.2.0
GCC_SHA256=b54e712dc3760d794203c2de9f0d4e31da601b1a40eee9fb943643006e0977f4

here=$(cd "$(dirname "$0")" && pwd)
out=$(mkdir -p "${1:-$here/build}" && cd "${1:-$here/build}" && pwd)
cd "$out"

if [ -z "${CROSS_COMPILE:-}" ]; then
	tc=x86_64-gcc-$GCC-nolibc-riscv64-linux.tar.xz
	if [ ! -d gcc-$GCC-nolibc ]; then
		[ -f $tc ] || curl -fLO \
			https://mirrors.edge.kernel.org/pub/tools/crosstool/files/bin/x86_64/$GCC/$tc
		echo "$GCC_SHA256  $tc" | sha256sum -c -
		tar xf $tc
	fi
	CROSS_COMPILE=$out/gcc-$GCC-nolibc/riscv64-linux/bin/riscv64-linux-
fi
major=$("${CROSS_COMPILE}gcc" -dumpversion | cut -d. -f1)
if [ "$major" -lt 14 ]; then
	echo "${CROSS_COMPILE}gcc is gcc $major; the function tracer needs gcc >= 14" >&2
	exit 1
fi
pahole=$(pahole --version 2>/dev/null | sed 's/^v//') || pahole=0
if [ "$(printf '%s\n1.22\n' "$pahole" | sort -V | head -1)" != 1.22 ]; then
	echo "BTF needs pahole >= 1.22 (dwarves), found ${pahole:-none}" >&2
	exit 1
fi
make="make -C $out/linux-$VER ARCH=riscv CROSS_COMPILE=$CROSS_COMPILE O=$out/obj"

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

# Check that every option of the fragment survived the dependencies,
# and that nothing is left as a module.
sed -n 's/^\(CONFIG_[A-Z0-9_]*\)=\(.*\)/\1=\2/p; s/^# \(CONFIG_[A-Z0-9_]*\) is not set/\1=n/p' \
	"$here/rsemu.config" | while IFS== read -r opt want; do
	got=$(sed -n "s/^$opt=//p" obj/.config)
	if [ "${got:-n}" != "$want" ]; then
		echo "$opt: want $want, got ${got:-n}" >&2
		exit 1
	fi
done
if grep '=m$' obj/.config >&2; then
	echo "options above are modules; set them in rsemu.config" >&2
	exit 1
fi

$make -j"$(nproc)" Image
echo "$out/obj/arch/riscv/boot/Image"
