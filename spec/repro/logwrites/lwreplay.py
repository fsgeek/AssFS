#!/usr/bin/env python3
"""Replay the first N entries of a dm-log-writes log onto a copy of an image.
Usage: lwreplay.py <log.img> <base.img> <N> <out.img> [skip-entry ...]"""
import shutil, sys, lwlog
log, base, n, out = sys.argv[1], sys.argv[2], int(sys.argv[3]), sys.argv[4]
skip = {int(x) for x in sys.argv[5:]}
shutil.copyfile(base, out)
with open(out, 'r+b') as f:
    for e in lwlog.entries(log):
        if e['i'] > n: break
        if e['i'] in skip or not e['data']: continue
        f.seek(e['sector'] * 512); f.write(e['data'])
