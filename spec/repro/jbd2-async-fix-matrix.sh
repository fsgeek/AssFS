#!/bin/sh
# Finding 16: the fix for finding 15, tested on kernel and e2fsck.
# Builds six images with debugfs (one/two transactions; intact or with
# fs block 1001's journal copy overwritten; async_commit set in the
# journal superblock or not), then recovers each with:
#   kernel: run.sh <bzImage> <img> [mntopts]   (QEMU, vm-init.sh as /init)
#   e2fsck: <e2fsck> -fy <copy>
# Usage: jbd2-async-fix-matrix.sh <workdir> ; see README finding 16.
set -e
here=$(cd "$(dirname "$0")" && pwd); cd "$1"
python3 -c "open('newAB','wb').write(b'A'*4096 + b'B'*4096); open('newC','wb').write(b'C'*4096)"
mkfs() { rm -f "$1"; mke2fs -q -F -t ext4 -O metadata_csum -b 4096 "$1" 64M; }
jblk() { debugfs -R "bmap <8> $2" "$1" 2>/dev/null; }
tear() { python3 -c "f=open('$1','r+b'); f.seek($2*4096); f.write(b'X'*4096)"; }
async() { python3 "$here/jsb-async.py" "$1" "$(jblk "$1" 0)"; }
mkfs one.img
printf 'jo -c\njw -b 1000,1001 newAB\njc\n' | debugfs -w -f - one.img >/dev/null 2>&1
cp one.img sync-ok.img; cp one.img sync-torn.img; tear sync-torn.img "$(jblk one.img 3)"
cp sync-ok.img async-ok.img; async async-ok.img
cp sync-torn.img async-torn.img; async async-torn.img
mkfs two.img
printf 'jo -c\njw -b 1000,1001 newAB\njc\njo -c\njw -b 1002 newC\njc\n' | debugfs -w -f - two.img >/dev/null 2>&1
cp two.img async-two-torn1.img; tear async-two-torn1.img "$(jblk two.img 3)"; async async-two-torn1.img
cp two.img async-two-ok.img; async async-two-ok.img
mkdir -p i/bin i/dev i/proc i/sys i/mnt; cp "$(command -v busybox)" i/bin/
cp "$here/vm-init.sh" i/init; chmod +x i/init
( cd i && find . | cpio -o -H newc 2>/dev/null | gzip > ../initramfs.gz )
cat > run.sh <<'RUN'
#!/bin/sh
k=$1; img=$2; opts=$3; cp "$img" "run-$$.img"
timeout 60 qemu-system-x86_64 -m 256M -nographic -no-reboot -kernel "$k" -initrd initramfs.gz \
  -append "console=ttyS0 loglevel=4 panic=-1 ${opts:+mntopts=$opts}" \
  -drive file="run-$$.img",format=raw,if=virtio 2>&1 | grep -a -E '^===|mount exit|JBD2|EXT4-fs \(vda\)|fs block'
rm -f "run-$$.img"
RUN
chmod +x run.sh
# IO-error case: T1 = 40 blocks (fs 1000-1039), T2 = {1050}; boot with
#   -drive file=blkdebug:blkdebug.conf:long.img,format=raw,if=virtio
# and vm-init-long.sh, where blkdebug.conf injects EIO (event read_aio,
# errno 5) at the sector of journal offset 38, past the first 128K
# readahead window so the descriptor still reads.
python3 -c "open('new40','wb').write(b''.join(bytes([65+i%26])*4096 for i in range(40)))"
mkfs long.img
printf "jo -c\njw -b $(seq -s, 1000 1039) new40\njc\njo -c\njw -b 1050 newC\njc\n" | debugfs -w -f - long.img >/dev/null 2>&1
async long.img
j=$(jblk long.img 38)
printf '[inject-error]\nevent = "read_aio"\nerrno = "5"\nsector = "%s"\nonce = "off"\n' $((j*8)) > blkdebug.conf
ls *.img
