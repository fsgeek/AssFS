#!/bin/sh
# run.sh <cache> <mntopts> <nops> : one boot, three runs of fsb
img=d-$$.img; log=l-$$.log; cp --sparse=never disk.img $img; sync $img
qemu-system-x86_64 -m 512M -nographic -no-reboot -kernel ../fix/bzImage-fixed2 -initrd initramfs-perf.gz \
  -append "console=ttyS0 loglevel=3 panic=-1 mntopts=$2 nops=$3" \
  -drive file=$img,format=raw,if=virtio,cache=$1 > $log 2>&1 &
q=$!; t=0
while ! grep -q BENCH-DONE $log && [ $t -lt 1500 ]; do sleep 2; t=$((t+2)); done
kill $q 2>/dev/null; wait $q 2>/dev/null
grep -a -E "^RESULT|MOUNT FAILED" $log | sed "s/^/cache=$1 opts=$2 /"; rm -f $img $log
