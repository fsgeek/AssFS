# Episode's log and recovery, model-checked

TLA+ models of the Episode file system's transaction log and recovery
(Transarc, 1991–96; source in `dce/dce/src/file/episode/logbuf`), checked
against two device contracts: the 1992 one (a completed write is on
media) and NVMe with a volatile write cache (a completed write is durable
only after a flush issued after it completed).

**Method.** Each mechanism is modelled from the code as written, not from
the design intent — the gap between the two is where the bugs were. Every
model is paired with deliberately broken variants that must fail, so a
pass means something. Crashes are not modelled as an action where it can
be avoided: the invariant asks "if power failed in this state, would
recovery be right?" in every reachable state. Bounds are small (2–3
transactions, 2–5 pages or log slots) and stated per table.

Run: `java -cp tla2tools.jar tlc2.TLC -deadlock -config <cfg> <module>.tla`
(ECDrain without `-deadlock`: deadlock is one of its results).

## Summary

**Bugs in the code as written**

| # | What | Consequence |
|---|---|---|
| 1 | `io_window` stores a count of in-transit pages; recovery reads it as a distance | spurious salvage when log pages complete out of order |
| 8 | A crash right after a clean mount leaves pages whose pass number the next mount reuses | **silent**: two runs' logs replayed as one |
| 9 | Masked pass comparison assumes all pages are within half the pass range; restarting at slot 0 breaks that | old page read as newer (argued; ~10⁴ mounts at 16 bits) |

**The 1992 assumption on modern hardware.** "Completion means durable"
(`recover.c:894`, wam 920624) was the device contract of its day. On a
volatile write cache it fails three ways, depending on what rests on it:
an acknowledged commit lost (2), a stolen page outrunning its undo record
(2), and another owner's data appearing in a file via undo-zero (5).

**Lying drives.** Old/new logging masks a lost write until checkpoint
truncation discards the redo record; then the loss is silent (4). A
version held by the parent detects it; reading back before truncating
repairs it.

**Design properties confirmed**

- Early lock release is safe because recovery stops at the first log gap (3).
- Undo-zero's "logically ordered" requirement is load-bearing (5).
- Equivalence-class merge as coded is correct; at full contention a class
  commit is a checkpoint, at low contention classes commit independently (6).
- Up-front log reservation is what lets a hot class always drain; on-demand
  allocation deadlocks (7).
- Physical old/new logging tolerates torn pages with no doublewrite buffer;
  LSN-gated delta redo does not (12).
- The `B_ASYNC` precondition in `efs_newalloc.c` holds (13).

**A repaired mount path.** With a bounded log, a finite pass number is
safe if nothing stale stays visible (10). Resume after the recovered head,
bound the writer's distance to the oldest unfinished page, and durably
overwrite that many slots at mount: passes even with a 3-bit pass field.
`io_window` must be a distance everywhere (11).

**Open**

- Vnode-lock audit for `epia_MarkInTransit`'s unasserted assumption (13).
- `resume-chain-long` salvage, undiagnosed (superseded by the scrub design).
- The `force` admission bypass for undo-zero (7) — caller's size promise unmodelled.
- Findings 1 and 8 need out-of-order log page completion; whether 1990s
  I/O paths ever produced it is not established here.

# Details

## StealNoForce.tla — the protocol, one log record per block

| cfg | WAL | flush snapshot | result |
|---|---|---|---|
| correct | yes | yes | passes (92,042 states) |
| nowal | no | yes | Atomicity: stolen page on media, its log record not |
| nosnap | yes | no | Atomicity: log write completing *during* a flush counted as stable |

## EpisodeLog.tla — Episode's paged log, as in `dce/.../episode/logbuf`

Pages sealed once (no tail rewrite), `io_window` in the page header,
SafeDumpLog's head finding, bail to salvager on an invalid page.

| cfg | device | io_window | stable means | result |
|---|---|---|---|---|
| ep1992 | write-through | count (`inTransit.length`) | completed | **NoSalvage fails**: fail-stop |
| ep1992-dist | write-through | distance to oldest in transit | completed | passes (39,289 states) |
| ep-nvme-dist | volatile cache | distance | completed | **Durability fails**: silent loss of an acked commit |
| ep-nvme-fixed | volatile cache | pages not yet flushed | flushed | passes (592,522 states) |

### Findings

1. **io_window is a count, recovery treats it as a distance.**
   `log.c:2990` stores `inTransit.length`; `RecycleLogBufs` (`log.c:2554`)
   removes *any* completed page from `inTransit`, so an older page can
   still be in transit after newer ones complete. Recovery
   (`recover.c:914`) checks only `io_window` pages back from the head, so
   the missing older page lands inside the scanned range and recovery
   raises `RCVR_IOERR` → salvager. Fail-stop, not corruption. The right value
   is already computed at the end of `RecycleLogBufs`: `activeLogBlock`,
   the oldest in-transit page. Store `currentLogBlock - activeLogBlock`.
   Caveat: latent if the I/O layer completes a device's writes in order.

2. **Completion ≠ durability.** `recover.c:894` (wam 920624): "we trust
   the underlying system to really guarantee that a page is on disk when
   it says it is". True for the 1992 contract; false for NVMe with a
   volatile write cache. Result: an acknowledged commit is silently lost,
   and (StealNoForce/nosnap) a stolen page can outrun its undo record.
   Fix: stable = covered by a flush *issued after* completion; io_window =
   pages not yet known stable.

3. Early lock release (drop locks when END is appended, before it is
   durable) is safe given recovery stops at the first gap.

## LostWrite.tla — checkpoint/truncation over a lying device

StealNoForce (correct variant) plus a header block holding the truncation
point, and a device that may report a page write complete without
storing it.

| cfg | truncation | lost writes | parent-held version | result |
|---|---|---|---|---|
| lw-honest | yes | 0 | no | passes (122,914 states) |
| lw-notrunc | no | 1 | no | passes (160,166 states): redo masks the lie |
| lw-trunc | yes | 1 | no | **Atomicity fails**: silent loss of a committed update |
| lw-detect | yes | 1 | yes | Atomicity/Durability pass (192,438 states)... |
| lw-detect-ns | yes | 1 | yes | ...but NoSalvage fails: detected, fail-stop |

4. **Old/new value logging masks lost writes until you truncate.** The
   redo record is the second copy. Truncation discards it on the
   strength of the drive's word, and the lost write becomes permanent and
   silent. A per-block CRC can't see it (the stale block is internally
   consistent). A version held by the parent (here: expected page LSN in
   the checkpoint header; ZFS: checksum in the block pointer; content
   addressing: the hash in the index) turns it into a detected failure.
   Episode checksummed log pages (CRC + pass number, which is the same
   idea for the log) but not metadata pages.

## UndoZero.tla — allocate without synchronous zero-fill

Txn A allocates block b (map free→F) and ends promptly; txn Z carries an
OFILL record (undo = zero b) and ends when the user's data write is done.
Property NoExposure: if recovery says b belongs to the file, b holds zeros
or the user's data — never the previous owner's detritus.

| cfg | device | Z ends when data write is | log order | result |
|---|---|---|---|---|
| uz-1992 | write-through | completed | OFILL before A.end | passes |
| uz-misorder | write-through | completed | A.end before OFILL | **exposure**: A durable, OFILL not |
| uz-nvme | volatile cache | completed | OFILL before A.end | **exposure**: Z.end durable, data in cache only |
| uz-nvme-fixed | volatile cache | flushed | OFILL before A.end | passes |

5. The completion-means-durable assumption turns into a **confidentiality**
   failure here, not just a durability one: the file shows another
   owner's data. The "logically ordered" requirement in log.c (wam
   920805) is load-bearing: OFILL must precede A's END in the log.

## ECMerge.tla — equivalence-class merge, modelled from tran.c

No page locks: writing a page whose last writer hasn't completed merges
the two classes (smaller folds into larger, heads chain). `ecSize` counts
members that haven't ended; the last ender writes a BEND per member under
the head's tranId, then the head's END. Recovery honours a BEND only if
its head has an END (HandleBulkEnd). No runtime abort.

| cfg | pages | variant | result |
|---|---|---|---|
| ec-episode | 2 | as coded | passes (7,518,392 states; 3 txns × 2 updates) |
| ec-nomerge | 2 | no merge | **Atomicity**: committed overwrite of an uncommitted page is undone |
| ec-bendown | 2 | BEND commits without head END | **Atomicity**: crash between BENDs and END splits the class |
| ec-double | 2 | merge counts pending members (AssFS bug) | **NoStuck**: class can never commit |
| ec-hot | 1 | as coded | ConsistencyPoint holds (88,478 states) |
| ec-cool | 2 | as coded | ConsistencyPoint fails: independent classes commit independently |

6. **At full contention an EC commit is a checkpoint.** On a single hot
   page every class commit leaves no writer uncommitted — the committed
   state is the whole in-memory state. With independent pages, classes
   commit separately. One mechanism, no mode switch.

## ECDrain.tla — can a hot class always drain?

Transactions recycle, so arrivals are unbounded and adversarial; fairness
only on writing, ending and recycling. Log space L units, one per record;
a class's units are released when it commits.

| cfg | space | L | result |
|---|---|---|---|
| drain-reserve | reserved at Begin | 4 | Drain holds (every pending txn commits) |
| drain-ondemand | taken per record | 3 | **deadlock**: log full of the class's own pinned records; last ender has no room for BENDs + END |
| drain-ondemand-big | taken per record | 6 | holds: L covers every slot's worst case |

7. **Up-front reservation is what makes the high-load checkpoint
   behaviour safe.** Under contention the class grows until log space
   stops admissions; it then drains and commits — a checkpoint forced by
   the log. That only works if every member already holds the space it
   needs to finish. The `force` bypass in log.c (wam 920805: "dangerous,
   but necessary to cascade UndoZero transactions") is exactly the
   exception to this argument, guarded by the caller's promise about
   combined size.

## WrapLog.tla — the circular log across mounts

Mount as elbl_Init/elbr_GetCleanLogInfo: slot 0 starting with a RESTART
record means clean; passno = IncPassno(restart pass) and writing restarts
at slot 0. Otherwise SafeDumpLog (binary search on masked pass distance,
window check in distance form, scan tail..head), then a checkpoint block
after the head and a restart block with pass + 2. Write-through device,
out-of-order completion, crashes anywhere. Ghost fields record each
page's mount incarnation.

| cfg | pass bits | mounts | FIX | result |
|---|---|---|---|---|
| wrap-asis | 4 | 3 | no | **NoSilentError fails**: two incarnations' logs replayed as one |
| wrap-fix | 4 | 3 | yes | passes (3,362 states) |
| wrap-fix-long | 3 | 5 | yes | NoSalvage fails: pass-number aliasing (see 9) |

8. **Pass number reuse after a crash on a clean mount.** Mount 1 reads
   restart pass R, writes pass R+1 from slot 0; slot 1 lands before slot
   0; crash. Nothing was acknowledged and slot 0 still holds the clean
   restart record, so mount 2 skips recovery — correctly — and starts
   pass R+1 *again*. Its slot 0 lands; crash before its slot 1 does.
   Media: slot 0 = mount 2's page, slot 1 = mount 1's page, same pass,
   valid checksum. Recovery accepts both: a silent splice of two
   histories. The recovery path's "+2 ... paranoia" bump (recover.c,
   elbr_Recover step 5) shows the hazard was anticipated; the clean-mount
   path has no bump. FIX: persist the new pass (rewrite the restart
   block) before writing any page with it. Same reordering caveat as 1.

9. **Masked pass comparison assumes all pages are within half the pass
   range.** Restarting at slot 0 on every mount leaves slots beyond short
   incarnations unrewritten, holding old pass numbers while the counter
   advances (+1 per clean mount, +3 per recover-and-mount). With a 3-bit
   field the model aliases within five mounts and the binary search reads
   an old page as newer. With the real 16-bit field this needs on the
   order of 10^4 mounts with some slot never rewritten — rare in
   production, plausible under crash-test harnesses. Not reproducible at
   W = 16 by model checking; stated as an argument, not a result.

## WrapResume.tla — resume after the recovered head

Tony's alternative: resume writing after the recovery point instead of
restarting at slot 0. Clean shutdown writes its restart record as the
next page in sequence; every mount runs the head search. Options: CHAIN
(each page names its predecessor) and SCRUB (at mount, durably overwrite
the IOMAX slots after the head with filler pages carrying the previous
pass). IOMAX bounds the io_window *distance* in the writer.

| cfg | N | pass bits | mounts | option | result |
|---|---|---|---|---|---|
| resume | 4 | 4 | 3 | none | **NoSilentError fails**: stale page, same (slot, pass), read as newer than the restart record |
| resume-chain | 4 | 4 | 3 | CHAIN | passes (7,960 states) |
| resume-chain-long | 4 | 3 | 5 | CHAIN | NoSalvage fails (not diagnosed; superseded) |
| resume-noscrub5 | 5 | 4 | 3 | none | NoSilentError fails |
| resume-scrub | 5 | 4 | 3 | SCRUB | passes (5,390 states) |
| resume-scrub-long | 5 | 3 | 5 | SCRUB | passes (73,947 states) |

10. **A finite pass number suffices if nothing stale stays visible**
    (Tony: with a bounded log there is a generation width that is never
    *visibly* reused). Episode's steady state has that property; the
    mount path broke it twice — restarting at slot 0 (unbounded
    lifetime for unrewritten slots, finding 9) and reusing names of a
    crashed run's pages ahead of the head (finding 8). The design that
    passes even with a 3-bit pass field: (a) resume after the head, so
    every slot rotates; (b) the writer bounds the distance to the oldest
    unfinished page; (c) mount durably overwrites that many slots ahead
    before writing. No chain, no persisted bump.

11. **Count vs distance, again.** Bounding the *number* of pages in flight
    is not enough: with one page stuck in flight, later pages keep
    completing, and a stale page sat three slots past the head with only
    two ever in flight. io_window must be a distance everywhere — the
    header, the writer's limit, and the mount-time scrub.

Model errors found and fixed during this section (each produced a
counterexample or a hollow pass that was inspected before trusting it):
shutdown not distinguished from crash; a frame condition that disabled
Submit (22-state "pass"); checkpoint/restart pages exempted from the
chain; the tail's own back-link checked against an overwritten slot.

## TornPage.tla — torn page writes vs. redo style

A page is a header sector (page LSN) plus data sectors; a page write lands
sector by sector, in any order. Single-record committing transactions,
WAL, any durable log prefix.

| cfg | redo | writes | result |
|---|---|---|---|
| torn-phys | physical old/new (Episode): set range to new bytes | can tear | passes (2,419 states) |
| torn-delta | "add 1", applied only if media page LSN is older | can tear | **Correct fails**: header sector lands, data sector doesn't; redo skipped |
| atomic-delta | same | atomic | passes (control) |

12. **Physical byte-range logging tolerates torn pages; LSN-gated delta
    redo does not.** Physical redo never consults the page, so any mix of
    old and new sectors converges — provided every change to the page is
    logged. That is why Episode needed no doublewrite buffer and why a
    physiological log (PostgreSQL) needs full_page_writes.
    Undo-zero under torn data writes, by argument only: Z ends after the
    data write *completes* (all sectors, on the 1992 contract), and undo
    zeroes the whole range regardless of which sectors landed.
    Caveat for a modern design: a per-page checksum (finding 4) will fail
    on a torn page *before* redo repairs it, so recovery must redo first
    and verify after, for pages the log covers.

## The B_ASYNC comment (vnops/efs_newalloc.c) — code reading, no model

The comment above efs_ZeroBytes: B_ASYNC is safe only if epia_Strategy
checks the pending-blocks list and waits on/sets INTRANSIT for every block
it writes.

13. **The stated precondition holds, and the call site doesn't rely on
    it.** anode/strategy.c epia_MarkInTransit does exactly that: for each
    block, wait while BUSY, then mark BUSY on writes; epia_ClearInTransit
    clears at iodone. And efs_ZeroBytes → efs_PutContentsNoVM is
    synchronous anyway (epia_BioWait; B_ASYNC never set).
    The assumptions moved one level down, and MarkInTransit states them:
    - "only one thread will find itself waiting for a particular
      in-transit disk block" — **asserted** (a second waiter panics:
      fail-stop);
    - "the blocks associated with this buf will not become in transit
      as a result of concurrent activities by other threads ... The
      higher-level vnode locking should guarantee this" — **not
      asserted**. For writes it is moot (the block is marked BUSY right
      after the wait). For reads it is load-bearing: a reader waits for
      block d, does not mark it, moves on, and d can enter transit again
      before the read is issued.
    Whether every epia_Strategy caller holds the vnode lock is a
    call-graph audit (VM paging vs. promotion/allocation/volume ops),
    not a model-checking question. Not done.

# Linux jbd2 (mainline 7.3-rc7, af32da4, 2026-10-09)

## JbdFirstCommit.tla — first commit after a clean mount

Modelled from fs/jbd2/{recovery,journal,commit}.c and fs/ext4/super.c.
Clean mount (s_start == 0) reuses s_sequence + 1 at the same head and
defers the superblock write to the first commit (JBD2_FLUSHED), which
writes it with no flags ("ok for write to be flushed only with
transaction commit"). One transaction per mount: descriptor, B data
blocks, commit block. Volatile write cache. Recovery as do_one_pass with
csum v3.

| cfg | commit mode | result |
|---|---|---|
| jbd-sync | default: commit block PREFLUSH\|FUA after data | passes (3,123 states) |
| jbd-async | journal_async_commit: commit block submitted without flush before data is waited on | **fails**: partial replay |

14. **Default jbd2 has Episode's finding-8 shape, made invisible.** The
    sequence and head are reused after a crash before the first commit,
    but the commit block's PREFLUSH makes the superblock durable before
    any commit block can be, so a stale commit with the reused sequence
    can never be on media while the superblock still says clean. Reuse,
    never visible — the principle of finding 10, by flush ordering.
    After a real recovery the superblock is rewritten with REQ_FUA
    before new transactions ("must make sure information about current
    log tail is on disk before that") — the Episode fix, already there.

15. **Candidate: journal_async_commit + metadata_csum can partially
    replay a transaction that never committed.** async_commit's safety
    rests on the commit block's checksum covering the transaction
    (commit.c passes crc32_sum, computed only for COMPAT_CHECKSUM, v1).
    With metadata_csum, ext4 selects CSUM_V3 instead
    (ext4_set_journal_csum_feature_set); the v3 commit checksum covers
    the commit block only, and PASS_SCAN does not verify data-block tags
    (calc_chksums is v1-only). If the commit block lands before a data
    block and power fails, scan accepts the transaction; PASS_REPLAY
    writes every tagged block whose checksum matches, skips the rest
    (EFSBADCRC), and recovery still sync_blockdev()s and flushes. Mount
    then fails, but the filesystem already holds a torn, never-
    acknowledged transaction. Scope: non-default option, refused with
    data=ordered (allowed with data=writeback/journal).
    Status: **recovery half reproduced** with e2fsprogs 1.47.0's copy of
    jbd2 recovery (`repro/jbd2-partial-replay.sh`, no root): of a
    two-block transaction, block 1000 is replayed and block 1001 is
    skipped with "Invalid checksum recovering data block 1001 in log".
    **Kernel: reproduced on mainline 7.3-rc7 (24a19b953)** in QEMU
    (`repro/jbd2-partial-replay-kernel.sh`): "JBD2: Invalid checksum
    recovering data block 1001 in journal block 3 / journal recovery
    failed / EXT4-fs (vda): error loading journal"; block 1000 replayed,
    block 1001 not. Not silent — the mount is refused and fsck is forced
    — but the torn, never-acknowledged transaction is already on disk.
    Writer half: **observed** with dm-log-writes on stock 7.3-rc7 (see 17).
    Prior art (archive search, lore not machine-searchable): the pieces
    are each known — async commit's safety was argued from the v1
    whole-transaction crc (Ts'o 2009, 0e3d2a6313d0); v3 drops COMPAT_CHECKSUM
    (Wong 2014); async_commit forbidden with data=ordered for a different
    reason (Kara 2014, d4f761074353); per-block skip proposed as a feature
    (Dilger 2021); Ts'o (bugzilla 93031, 2015) called async_commit "not
    safe" without saying why. Nothing found combining them.

## JbdAsync.tla — async commit across transactions, and the fix

Several transactions committed in turn (kjournald2 does not start
transaction n+1 until n's trailing flush returns), each as
journal_async_commit writes it: descriptor, data and commit block with no
flush, wait, `blkdev_issue_flush`, acknowledge. Optional media corruption
of one data block of an acknowledged transaction. Recovery as written
(FIX = FALSE) or with the proposed PASS_SCAN tag check (FIX = TRUE).
Invariants: no transaction partly replayed (Atomic); a successful mount
has every acknowledged transaction (Durable); no mount failure without
media corruption (NoFalseAlarm).

| cfg | mode | result |
|---|---|---|
| jasync-asis | async, as written | **fails** |
| jasync-fix | async, fix | passes, T=3 (247 states) |
| jasync-sync | sync commit, as written | passes |
| jasync-fix-corrupt | async, fix, media corruption | **fails Durable**: corrupt last txn dropped |
| jasync-asis-corrupt | async, as written, media corruption | **fails** |

The passes are small but not hollow. Probe invariants confirmed that the
fix model reaches truncation of a torn transaction behind an intact one,
a full log, and (with corruption) the "later commit means corrupt" error.

16. **Fix for 15: check the tags in PASS_SCAN under async_commit.**
    Restores what the v1 design gave async commit. If a transaction's tag
    checksums fail, end the log before it (an interrupted commit). If a
    later commit block follows, the failed transaction must have been
    flushed, so it is corrupt: set `j_failed_commit` and refuse the mount,
    as v1 does. The cost is the one v1 async already pays: corruption
    of the *last* transaction's data looks exactly like an interrupted
    commit, so that acknowledged transaction is dropped, with a notice
    (jasync-fix-corrupt). Sync commit is untouched.
    Patches: `fix/0001-jbd2-*.patch` (7.3-rc7) and `fix/0001-e2fsck-*.patch`
    (e2fsprogs next, 8fd7952). Tested on images from
    `repro/jbd2-async-fix-matrix.sh` (async flag set in the journal
    superblock by `repro/jsb-async.py`), on stock and patched kernels in
    QEMU and on stock and patched e2fsck (`fix/*-results.txt`):

    | image | stock | patched |
    |---|---|---|
    | async, T1 torn | 1000 replayed, 1001 not; mount refused | nothing replayed; mount OK, notice |
    | async, T1 torn, T2 valid | T1 half **and T2** replayed; refused | nothing replayed; "transaction 1 is corrupt"; refused |
    | async intact (1 or 2 txns) | replayed | replayed |
    | sync torn / intact | refused / replayed | unchanged |
    | async, T1 = 40 blocks, T2 valid, EIO on one T1 block (blkdebug) | T1 minus that block **and T2** replayed; refused | nothing replayed; recovery fails |

    An independent review of the first version found a regression, which
    the run above confirms (`fix/kernel-eio-results.txt`, "fixed"): an IO
    error during the new scan check `break`s out of the switch only, so
    the scan resumed at the next block and ended there, and the mount
    *succeeded* with both committed transactions silently dropped. (The
    v1 `calc_chksums` path has the same shape.) Fixed: the error now
    fails recovery. The review also found that `jbd2_journal_wipe()`
    would refuse the mount, since `skip_recovery` runs PASS_SCAN and could
    now set `j_failed_commit`; `skip_recovery` now clears it.
    Left as is: tid 0 doubles as "unset" for `end_transaction`, as it
    already does in v1.

    Not done: xfstests.

## dm-log-writes — the writer, captured (no model)

17. **The torn state comes from the real writer, not only from a
    reordering cache.** `repro/logwrites/run.sh` mounts ext4
    (metadata_csum, `journal_async_commit,data=writeback`) through
    dm-log-writes on stock 7.3-rc7 in QEMU and runs six fsyncs; the log
    records every write in completion order with its flags
    (`fix/logwrites-timeline.txt`). Every transaction's descriptor, data
    and commit block share one flush epoch, the commit block carrying no
    flags. In transaction 4 the commit block (entry 29) *completed before*
    its descriptor (30) and six of its seven data blocks (31-36), so a plain
    prefix of the completion order — what a write-through disk could hold
    after a crash, the xfstests replay-log method — tears it.
    Replaying every prefix and running e2fsck (`fix/logwrites-e2fsck-prefixes.txt`):
    prefixes 30-35 are exactly the window. Stock e2fsck replays part of
    transaction 4 and then, depending on the prefix, finds an unattached
    inode, an unconnected directory, a corrupted directory, or nothing:
    at prefix 30 inode 17 is a live 16 KB file in no directory and no
    bitmap, hidden from fsck because the group descriptor (which would
    raise the itable high-water mark) was one of the skipped blocks.
    Patched e2fsck: "treating as interrupted async commit" and only the
    free-count fixes every crash image gets. Kernel on prefix 33
    (`fix/logwrites-kernel-prefix33.txt`): stock fails recovery, leaving
    "Directory inode 16 ... directory corrupted"; patched mounts and a
    read-only e2fsck afterwards is clean.

