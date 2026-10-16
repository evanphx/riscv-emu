# Builds vmagent and vmagent.cpio, an initramfs with vmagent as /init.
CC := riscv64-linux-gnu-gcc
CFLAGS := -Os -Wall -Wextra -static

all: vmagent.cpio

vmagent: vmagent.c
	$(CC) $(CFLAGS) -o $@ $<
	riscv64-linux-gnu-strip $@

vmagent.cpio: vmagent
	rm -rf initramfs && mkdir initramfs
	cp vmagent initramfs/init
	cd initramfs && echo init | cpio -o -H newc -R 0:0 --quiet > ../$@
	rm -rf initramfs

clean:
	rm -rf vmagent vmagent.cpio initramfs
