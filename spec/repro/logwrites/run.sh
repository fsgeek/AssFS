#!/bin/sh
# Writer half of finding 15, on the real kernel: capture ext4 + jbd2
# (metadata_csum, journal_async_commit, data=writeback) through
# dm-log-writes in QEMU, then replay every prefix of the completion-order
# log onto the base image and fsck it (the xfstests replay-log method).
# Usage: run.sh <bzImage with MD, BLK_DEV_DM, DM_LOG_WRITES> <e2fsck>...
# The capture kernel was 7.3-rc7 with stock fs/jbd2/recovery.c.
set -e
here=$(cd "$(dirname "$0")" && pwd); k=$1; shift; w=$(mktemp -d); cd "$w"
mke2fs -q -F -t ext4 -O metadata_csum -b 4096 base.img 64M
cp base.img data.img; truncate -s 256M log.img
mkdir -p i/bin i/dev i/proc i/sys i/mnt i/lib/x86_64-linux-gnu i/lib64
cp "$(command -v busybox)" i/bin/; cp "$(command -v dmsetup)" i/bin/
for l in $(ldd "$(command -v dmsetup)" | grep -o '/[^ ]*'); do cp -L "$l" "i$l"; done
cp "$here/lw-init.sh" i/init; chmod +x i/init
( cd i && find . | cpio -o -H newc 2>/dev/null | gzip > ../initramfs-lw.gz )
timeout 150 qemu-system-x86_64 -m 256M -nographic -no-reboot -kernel "$k" \
  -initrd initramfs-lw.gz -append "console=ttyS0 loglevel=4 panic=-1" \
  -drive file=data.img,format=raw,if=virtio -drive file=log.img,format=raw,if=virtio \
  > console.log 2>&1 || true
python3 "$here/lwlog.py" log.img base.img > timeline.txt
n=$(grep -c . timeline.txt)
for i in $(seq 1 "$n"); do
  python3 "$here/lwreplay.py" log.img base.img "$i" p.img
  for e in "$@"; do cp p.img q.img; "$e" -fy q.img > q.log 2>&1 || true
    grep -q -E "Invalid checksum recovering|interrupted async" q.log &&
      echo "prefix $i $e: $(grep -v -E '^Pass|^e2fsck|recovering journal|^$|WAS MODIFIED|files \(' q.log | tr '\n' ' ' | cut -c1-200)"
  done
done | tee prefixes.txt
echo "workdir: $w"
