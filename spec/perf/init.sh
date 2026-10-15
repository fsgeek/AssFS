#!/bin/busybox sh
/bin/busybox --install -s /bin
mount -t proc proc /proc; mount -t sysfs sys /sys; mount -t devtmpfs dev /dev
opts=$(sed -n 's/.*mntopts=\([^ ]*\).*/\1/p' /proc/cmdline)
n=$(sed -n 's/.*nops=\([0-9]*\).*/\1/p' /proc/cmdline)
mount -t ext4 -o "$opts" /dev/vda /mnt || echo "MOUNT FAILED $opts"
echo "CONFIG opts=$opts"
for r in 1 2 3; do /bin/fsb /mnt/f$r "$n"; done
umount /mnt; echo BENCH-DONE; poweroff -f
