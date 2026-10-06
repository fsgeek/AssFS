# Specs

TLA+ models of Episode-style logging over modern device contracts.
Checked with TLC (2 pages, 2 transactions, 1 update each).

Run: `java -cp tla2tools.jar tlc2.TLC -deadlock -config <cfg> <module>.tla`

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
