#!/bin/sh
# Bare-metal fsb on a scratch partition at the end of /dev/nvme0n1 (gazelle).
# Usage: gazelle-bench.sh <path to fsb.c>.  Works in a private mktemp dir.
set -e
D=/dev/nvme0n1; S="sudo -n"
W=$(mktemp -d); M=$W/mnt
trap 'mountpoint -q "$M" && $S umount "$M"; rm -rf "$W"' EXIT
q() { grep -v "unable to resolve host" || true; }
cp "$1" "$W/fsb.c"; cd "$W" && gcc -O2 -o fsb fsb.c
echo "AC online: $(cat /sys/class/power_supply/A*/online 2>/dev/null | tr '\n' ' ')  load: $(cut -d' ' -f1-3 /proc/loadavg)"
lsblk -lno PARTLABEL $D | grep -qx fsbscratch ||
  $S parted -s $D unit MiB mkpart fsbscratch ext4 945600 953800 2>&1 | q
$S partprobe $D 2>&1 | q; sleep 2
PART=$(lsblk -lnpo NAME,PARTLABEL $D | awk '$2=="fsbscratch"{print $1}')
echo "partition: $PART $(lsblk -no SIZE $PART)"
mkdir "$M"
for o in data=writeback data=writeback,journal_async_commit data=ordered; do
  $S mkfs.ext4 -q -F -O metadata_csum -E lazy_itable_init=0,lazy_journal_init=0 $PART 2>&1 | q
  $S mount -t ext4 -o $o $PART "$M" 2>&1 | q; $S chown "$(id -u):$(id -g)" "$M"
  echo "CONFIG $o ($(grep " $M " /proc/mounts | cut -d' ' -f4))"
  for r in 1 2 3; do ./fsb $M/f$r 2000; done
  $S umount "$M" 2>&1 | q
done
N=$($S parted -s $D print 2>/dev/null | awk '/fsbscratch/{print $1}')
$S parted -s $D rm $N 2>&1 | q; $S partprobe $D 2>&1 | q
echo "cleanup: $($S parted -s $D print 2>/dev/null | grep -c fsbscratch) scratch partitions left"
