#!/bin/busybox sh
/bin/busybox --install -s /bin
mount -t proc proc /proc; mount -t sysfs sys /sys; mount -t devtmpfs dev /dev
echo "=== kernel $(uname -r)"
mount -t ext4 /dev/vda /mnt; echo "mount exit=$?"
dmesg | grep -i -E 'jbd2|ext4|checksum' 
umount /mnt 2>/dev/null
for b in 1000 1001; do
  echo "fs block $b: $(dd if=/dev/vda bs=4096 skip=$b count=1 2>/dev/null | od -An -c -N4 | tr -s ' ')"
done
poweroff -f
