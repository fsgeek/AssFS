#!/usr/bin/env python3
# Set JBD2_FEATURE_INCOMPAT_ASYNC_COMMIT in an ext4 image's internal journal
# superblock and recompute its crc32c. Usage: jsb-async.py <img> <journal-block-0>
import struct, sys
T = []
for i in range(256):
    c = i
    for _ in range(8):
        c = (c >> 1) ^ 0x82F63B78 if c & 1 else c >> 1
    T.append(c)
def crc32c(seed, data):           # kernel-style: no pre/post inversion
    c = seed
    for b in data:
        c = T[(c ^ b) & 0xff] ^ (c >> 8)
    return c
img, blk = sys.argv[1], int(sys.argv[2])
with open(img, 'r+b') as f:
    f.seek(blk * 4096); sb = bytearray(f.read(1024))
    assert sb[:4] == b'\xc0\x3b\x39\x98'
    inc = struct.unpack_from('>I', sb, 0x28)[0] | 0x4
    struct.pack_into('>I', sb, 0x28, inc)
    struct.pack_into('>I', sb, 0xFC, 0)
    struct.pack_into('>I', sb, 0xFC, crc32c(0xffffffff, sb))
    f.seek(blk * 4096); f.write(sb)
