#!/bin/busybox sh
/bin/busybox --install -s /bin
mount -t proc proc /proc; mount -t sysfs sys /sys; mount -t devtmpfs dev /dev
echo "=== kernel $(uname -r)"
sz=$(blockdev --getsz /dev/vda)
dmsetup create lw --noudevsync --table "0 $sz log-writes /dev/vda /dev/vdb"; echo "dmsetup exit=$?"
dmsetup mknodes
mount -t ext4 -o journal_async_commit,data=writeback /dev/mapper/lw /mnt; echo "mount exit=$?"
dmsetup message lw 0 mark mounted
for i in 1 2 3 4 5 6; do
  mkdir -p /mnt/d$i
  dd if=/dev/zero of=/mnt/d$i/f bs=4096 count=4 conv=fsync 2>/dev/null
  echo "fsync $i done"
  dmsetup message lw 0 mark fsync$i
done
umount /mnt; echo "umount exit=$?"
dmsetup remove lw; echo "remove exit=$?"
dmesg | grep -i -E 'jbd2|ext4|device-mapper|log-writes' | tail -5
poweroff -f
