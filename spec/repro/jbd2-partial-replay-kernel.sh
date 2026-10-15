#!/bin/sh
# Finding 15 on a real kernel: build a minimal mainline kernel, boot it in
# QEMU (no KVM needed) with the torn image from jbd2-partial-replay.sh as
# /dev/vda; vm-init.sh mounts it (kernel journal recovery) and dumps
# fs blocks 1000/1001. Usage: jbd2-partial-replay-kernel.sh <linux-src> <img>
# Reproduced 2026-10-13 on 7.3-rc7 (24a19b953).
set -e
src=$1; img=$2; here=$(cd "$(dirname "$0")" && pwd); w=$(mktemp -d)
( cd "$src" && make -s tinyconfig && ./scripts/config -e 64BIT -e PRINTK -e TTY \
    -e SERIAL_8250 -e SERIAL_8250_CONSOLE -e BLK_DEV_INITRD -e RD_GZIP -e DEVTMPFS \
    -e DEVTMPFS_MOUNT -e BINFMT_ELF -e BINFMT_SCRIPT -e PROC_FS -e SYSFS -e BLOCK \
    -e PCI -e VIRTIO_MENU -e VIRTIO -e VIRTIO_PCI -e VIRTIO_BLK -e EXT4_FS -e JBD2 \
    -e CRYPTO -e CRYPTO_CRC32C -e LIBCRC32C -e CRC16 -e MULTIUSER -e FUTEX -e EPOLL \
    -e SHMEM && make -s olddefconfig && make -s -j"$(nproc)" bzImage )
mkdir -p "$w/i/bin" "$w/i/dev" "$w/i/proc" "$w/i/sys" "$w/i/mnt"
cp "$(command -v busybox)" "$w/i/bin/"; cp "$here/vm-init.sh" "$w/i/init"; chmod +x "$w/i/init"
( cd "$w/i" && find . | cpio -o -H newc 2>/dev/null | gzip > "$w/initramfs.gz" )
timeout 120 qemu-system-x86_64 -m 256M -nographic -no-reboot \
  -kernel "$src/arch/x86/boot/bzImage" -initrd "$w/initramfs.gz" \
  -append "console=ttyS0 loglevel=4 panic=-1" \
  -drive file="$img",format=raw,if=virtio 2>&1 | grep -a -E 'kernel|JBD2|EXT4-fs|fs block' || true
