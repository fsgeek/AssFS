#!/bin/sh
# Finding 15, recovery half: a csum-v3 journal whose transaction has a
# valid descriptor and commit block but one data block with stale content
# (as if the commit block landed before that data block -- possible under
# journal_async_commit) is partially replayed: matching blocks are written,
# the mismatched one is skipped with "Invalid checksum".
#
# Userspace only (no root): uses e2fsprogs' copy of jbd2 recovery.
# Tested with e2fsprogs 1.47.0.
set -e
dir=$(mktemp -d); cd "$dir"
mke2fs -q -F -t ext4 -O metadata_csum -b 4096 fs.img 64M
python3 -c "open('newAB','wb').write(b'A'*4096 + b'B'*4096)"
printf 'jo -c\njw -b 1000,1001 newAB\njc\n' | debugfs -w -f - fs.img >/dev/null 2>&1
debugfs -R logdump fs.img 2>/dev/null | grep -E 'descriptor|commit'
j3=$(debugfs -R 'bmap <8> 3' fs.img 2>/dev/null)   # journal copy of fs block 1001
cp fs.img torn.img
python3 -c "f=open('torn.img','r+b'); f.seek($j3*4096); f.write(b'X'*4096)"
e2fsck -fy torn.img 2>&1 | grep -E 'recovering|Invalid|checksum' || true
python3 - <<'PY'
with open('torn.img','rb') as f:
    for b, want in ((1000, ord('A')), (1001, ord('B'))):
        f.seek(b*4096); got = set(f.read(4096))
        print(f"fs block {b}: {'replayed' if got == {want} else 'NOT replayed'} {sorted(got)}")
PY
echo "workdir: $dir"
