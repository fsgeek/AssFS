#!/usr/bin/env python3
"""Parse a dm-log-writes log; classify writes that land in an ext4 journal.
Usage: lwlog.py <log.img> <fs.img>   (journal map from debugfs stat <8>)"""
import re, subprocess
import struct, sys
FLUSH, FUA, DISCARD, MARK, META = 1, 2, 4, 8, 16
def entries(path):
    f = open(path, 'rb')
    magic, ver, n, ss = struct.unpack('<QQQI', f.read(28))
    assert magic == 0x6a736677736872, hex(magic)
    pos = 1                                   # in units of ss
    for i in range(n):
        f.seek(pos * ss); sector, nr, flags, dlen = struct.unpack('<QQQQ', f.read(32))
        mark = f.read(dlen).split(b'\0')[0].decode() if flags & MARK else None
        data = b''
        if not flags & (MARK | DISCARD) and nr:
            f.seek((pos + 1) * ss); data = f.read(nr * ss); pos += nr
        pos += 1
        yield dict(i=i + 1, sector=sector * ss // 512, nr=nr * ss // 512, flags=flags, mark=mark, data=data)
JT = {1: 'desc', 2: 'commit', 3: 'jsb1', 4: 'jsb2', 5: 'revoke', 6: 'fc'}
def jmap(img):
    """fs block -> journal offset, from the extents of inode 8."""
    st = subprocess.run(['debugfs', '-R', 'stat <8>', img], capture_output=True, text=True).stdout
    m = {}
    for a, b, c, d in re.findall(r'\((\d+)-(\d+)\):(\d+)-(\d+)', st):
        for k in range(int(b) - int(a) + 1):
            m[int(c) + k] = int(a) + k
    return m
def classify(e, jm):
    out = []
    for k in range(len(e['data']) // 4096):
        fsb = e['sector'] // 8 + k
        if fsb not in jm:
            out.append(('fs', fsb, None, None)); continue
        b = e['data'][k * 4096:(k + 1) * 4096]
        m, bt, seq = struct.unpack('>III', b[:12])
        out.append(('j', jm[fsb], JT.get(bt, '?') if m == 0xc03b3998 else 'data', seq if m == 0xc03b3998 else None))
    return out
def fl(f):
    return '|'.join(n for b, n in ((FLUSH, 'FLUSH'), (FUA, 'FUA'), (DISCARD, 'DISCARD'), (META, 'META')) if f & b) or '-'
if __name__ == '__main__':
    log, jm = sys.argv[1], jmap(sys.argv[2])
    for e in entries(log):
        if e['mark']:
            print(f"{e['i']:4} MARK {e['mark']}"); continue
        c = classify(e, jm) if e['data'] else []
        js = [x for x in c if x[0] == 'j']
        desc = ', '.join(f"j{x[1]}:{x[2]}" + (f"(seq {x[3]})" if x[3] is not None else '') for x in js)
        nfs = sum(1 for x in c if x[0] == 'fs')
        print(f"{e['i']:4} {fl(e['flags']):10} sec {e['sector']:7} n {e['nr']:3}  {desc}{'  fs x%d' % nfs if nfs else ''}")
