#!/bin/sh
# Builds node.ext4, the default root filesystem of rsemu-server:
# Debian trixie (slim) for riscv64 with Node.js and Python in /usr/local.
#
# usage: mkimage.sh [OUTDIR]   (default: ./build)
#
# Needs curl, jq, fakeroot, dpkg-deb, mke2fs and riscv64-linux-gnu-strip. The
# downloads are pinned by digest.
set -eu

# debian:trixie-slim, the linux/riscv64 image manifest.
DEBIAN_MANIFEST=sha256:c6d58429cec88e03b2fe5c3d285bf90517be50b20bc1151b1b5a436d05865a9d
# Node.js has no official riscv64 builds, and unofficial-builds.nodejs.org
# stopped at v26.0.0; Cloud-V builds current releases natively on RISC-V
# hardware.
NODE_VERSION=26.11.1
NODE_URL=https://github.com/Cloud-V-10xE/RISC-V-software/releases/download/release-2026-10-09/nodejs-$NODE_VERSION-riscv64-linux.tar.gz
NODE_SHA256=f4f8a22421fcbda7d6af6f6696e976c4bd327885c2e0b575e3f533d072d9089c
# CPython from python-build-standalone, which builds riscv64.
PYTHON_VERSION=3.13.16
PYTHON_URL=https://github.com/astral-sh/python-build-standalone/releases/download/20261013/cpython-$PYTHON_VERSION%2B20261013-riscv64-unknown-linux-gnu-install_only_stripped.tar.gz
PYTHON_SHA256=38acb9b81353adbf74fcee3b0dfd70d43605c26e126e9c4025060d6553342296
# It needs libatomic, which debian:trixie-slim does not have.
LIBATOMIC_URL=http://deb.debian.org/debian/pool/main/g/gcc-14/libatomic1_14.2.0-19_riscv64.deb
LIBATOMIC_SHA256=ebaf12b2de00a4b567f70c629c0ff744dc92491029b3219d59c05c733f200c65

here=$(cd "$(dirname "$0")" && pwd)
out=$(mkdir -p "${1:-$here/build}" && cd "${1:-$here/build}" && pwd)
cd "$out"

# The Debian layer, from the Docker Hub registry.
if [ ! -f debian.tar.gz ]; then
	repo=library/debian
	token=$(curl -fsS "https://auth.docker.io/token?service=registry.docker.io&scope=repository:$repo:pull" | jq -r .token)
	layer=$(curl -fsS -H "Authorization: Bearer $token" \
		-H 'Accept: application/vnd.oci.image.manifest.v1+json' \
		-H 'Accept: application/vnd.docker.distribution.manifest.v2+json' \
		"https://registry-1.docker.io/v2/$repo/manifests/$DEBIAN_MANIFEST" | jq -r '.layers[0].digest')
	curl -fsSL -H "Authorization: Bearer $token" -o debian.tar.gz.tmp \
		"https://registry-1.docker.io/v2/$repo/blobs/$layer"
	echo "${layer#sha256:}  debian.tar.gz.tmp" | sha256sum -c - >/dev/null
	mv debian.tar.gz.tmp debian.tar.gz
fi

fetch() { # file url sha256
	if [ ! -f "$1" ]; then
		curl -fsSL -o "$1.tmp" "$2"
		echo "$3  $1.tmp" | sha256sum -c - >/dev/null
		mv "$1.tmp" "$1"
	fi
}
node=node-$NODE_VERSION.tar.gz
python=python-$PYTHON_VERSION.tar.gz
fetch "$node" "$NODE_URL" "$NODE_SHA256"
fetch "$python" "$PYTHON_URL" "$PYTHON_SHA256"
fetch libatomic1.deb "$LIBATOMIC_URL" "$LIBATOMIC_SHA256"

# Unpack and make the image in one fakeroot session, so that the files
# are owned by root and the device nodes exist.
rm -rf root node.ext4
mkdir root
fakeroot sh -eu -c '
	tar xzf debian.tar.gz -C root
	tar xzf "$1" -C root/usr/local --no-same-owner
	tar xzf "$2" -C root/usr/local --no-same-owner --strip-components=1
	dpkg-deb -x libatomic1.deb root
	rm -rf root/usr/local/include
	riscv64-linux-gnu-strip root/usr/local/bin/node
	echo vm > root/etc/hostname
	printf "127.0.0.1\tlocalhost\n127.0.1.1\tvm\n::1\tlocalhost\n" > root/etc/hosts
	# The guest has no network; npm must not try to reach the registry
	# for update checks.
	printf "update-notifier=false\nfund=false\naudit=false\n" > root/root/.npmrc
	size=$(du -sm root | cut -f1)
	mke2fs -q -t ext4 -L rootfs -d root node.ext4 $((size * 5 / 4 + 64))M
' sh "$node" "$python"
rm -rf root
echo "$out/node.ext4"
