--------------------------- MODULE JbdFirstCommit ---------------------------
(***************************************************************************)
(* Linux jbd2 (mainline, Oct 2026): the first commit after a clean mount.   *)
(*                                                                          *)
(* fs/jbd2/recovery.c jbd2_journal_recover: s_start == 0 means clean; the   *)
(*   new sequence is s_sequence + 1, head unchanged; no superblock write    *)
(*   (journal.c: "defer the superblock update until the next commit by      *)
(*   setting JBD2_FLUSHED").                                                *)
(* fs/jbd2/commit.c: if JBD2_FLUSHED, update the superblock with no flags   *)
(*   ("ok for write to be flushed only with transaction commit") and wait  *)
(*   for completion; write descriptor + data blocks; then                   *)
(*     sync  : wait for them, write commit block with PREFLUSH|FUA;         *)
(*     async : write commit block with no flush first, wait for all, then   *)
(*             blkdev_issue_flush.                                          *)
(* Recovery (do_one_pass, csum v3): from s_start with s_sequence; a         *)
(*   descriptor with the expected sequence and a valid self-checksum, then  *)
(*   a commit block with the expected sequence and valid self-checksum,     *)
(*   commits the transaction; replay writes each tagged block whose         *)
(*   per-tag checksum matches and skips (reporting EFSBADCRC) those that    *)
(*   don't. After recovery the superblock is rewritten with FUA and the     *)
(*   sequence moves past the last transaction seen.                         *)
(*                                                                          *)
(* One transaction per mount: descriptor at H, data at H+1..H+B, commit at  *)
(* H+B+1. Device: volatile write cache; flush; FUA. Ghost field `mnt` names *)
(* the mount that wrote each block.                                         *)
(***************************************************************************)
EXTENDS Naturals, FiniteSets

CONSTANTS B, MaxMount, ASYNC, Nil

Data  == 1..B
Pos   == 0..(B + 1)                   \* 0 = descriptor, 1..B data, B+1 commit
CPos  == B + 1
Empty == [mnt |-> 0, seq |-> 0, kind |-> "empty"]

VARIABLES
    sbM, sbC,          \* superblock: media / volatile cache (Nil = none cached)
    jM, jC,            \* journal blocks: media / cache
    queue,             \* submitted, uncompleted writes
    mnt, seq, phase,   \* current mount, its transaction sequence, commit phase
    acked,             \* mounts whose transaction was acknowledged
    bad, why

vars == <<sbM, sbC, jM, jC, queue, mnt, seq, phase, acked, bad, why>>

SB(start, s) == [start |-> start, seq |-> s]

Init ==
    /\ sbM = SB(0, 10) /\ sbC = Nil          \* clean, last sequence 10
    /\ jM = [p \in Pos |-> Empty] /\ jC = [p \in Pos |-> Nil]
    /\ queue = {}
    /\ mnt = 0 /\ seq = 0 /\ phase = "down"
    /\ acked = {} /\ bad = FALSE /\ why = ""

Blk(k, m, s) == [mnt |-> m, seq |-> s, kind |-> k]

(* ---------------- device ---------------- *)
Complete(w) ==
    /\ w \in queue
    /\ queue' = queue \ {w}
    /\ IF w.what = "sb"
         THEN /\ IF w.fua THEN sbM' = w.val /\ sbC' = Nil
                          ELSE sbC' = w.val /\ UNCHANGED sbM
              /\ UNCHANGED <<jM, jC>>
         ELSE /\ IF w.fua THEN jM' = [jM EXCEPT ![w.pos] = w.val] /\ UNCHANGED jC
                          ELSE jC' = [jC EXCEPT ![w.pos] = w.val] /\ UNCHANGED jM
              /\ UNCHANGED <<sbM, sbC>>
    /\ UNCHANGED <<mnt, seq, phase, acked, bad, why>>

DestageSb == /\ sbC # Nil /\ sbM' = sbC /\ sbC' = Nil
             /\ UNCHANGED <<jM, jC, queue, mnt, seq, phase, acked, bad, why>>
DestageJ(p) == /\ jC[p] # Nil /\ jM' = [jM EXCEPT ![p] = jC[p]] /\ jC' = [jC EXCEPT ![p] = Nil]
               /\ UNCHANGED <<sbM, sbC, queue, mnt, seq, phase, acked, bad, why>>

FlushAll(m, c, s, sc) ==   \* everything in cache to media
    /\ m' = [p \in Pos |-> IF c[p] # Nil THEN c[p] ELSE jM[p]] /\ c' = [p \in Pos |-> Nil]
    /\ s' = IF sc # Nil THEN sc ELSE sbM

(* ---------------- recovery, as do_one_pass (csum v3) ---------------- *)
Desc    == jM[0]
Commit  == jM[CPos]
Scanned == sbM.start # 0 /\ Desc.kind = "desc" /\ Desc.seq = sbM.seq + 1
Committed == Scanned /\ Commit.kind = "commit" /\ Commit.seq = sbM.seq + 1
\* Tag checksum: tag i in the descriptor is for that mount's block i.
TagOK(i) == jM[i].kind = "data" /\ jM[i].mnt = Desc.mnt
Replayed == IF Committed THEN {i \in Data : TagOK(i)} ELSE {}
Errors   == Committed /\ \E i \in Data : ~TagOK(i)

\* What should happen: replay exactly the latest crashed mount's committed
\* transaction, all of it, or nothing.
RightOutcome ==
    IF Committed
      THEN Desc.mnt = Commit.mnt /\ Replayed = Data
      ELSE mnt \notin acked

(* ---------------- mount ---------------- *)
Mount ==
    /\ phase = "down" /\ mnt < MaxMount
    /\ IF sbM.start = 0
         THEN /\ seq' = sbM.seq + 1                  \* clean: no sb write
              /\ UNCHANGED <<sbM, sbC, bad, why>>
         ELSE /\ bad' = (bad \/ ~RightOutcome)
              /\ why' = IF ~RightOutcome
                          THEN IF Committed /\ Desc.mnt # Commit.mnt
                                 THEN "spliced: descriptor and commit from different mounts"
                                 ELSE IF Committed THEN "partial replay"
                                 ELSE "acked transaction lost"
                          ELSE why
              \* after recovery: sb rewritten with FUA, sequence past last seen
              /\ LET last == IF Committed THEN sbM.seq + 1 ELSE sbM.seq
                 IN /\ sbM' = SB(1, last + 1) /\ seq' = last + 2   \* FUA, s_start set
              /\ sbC' = Nil
    /\ mnt' = mnt + 1
    \* clean: JBD2_FLUSHED, first commit writes the sb; recovered: already written
    /\ phase' = IF sbM.start = 0 THEN "sb" ELSE "sbwait"
    /\ UNCHANGED <<jM, jC, queue, acked>>

(* ---------------- the first commit ---------------- *)
\* JBD2_FLUSHED: superblock update, no flags, wait for completion.
WriteSb ==
    /\ phase = "sb"
    /\ queue' = queue \cup {[what |-> "sb", pos |-> 0, fua |-> FALSE,
                             val |-> SB(1, seq - 1)]}
    /\ phase' = "sbwait"
    /\ UNCHANGED <<sbM, sbC, jM, jC, mnt, seq, acked, bad, why>>

SbDone ==
    /\ phase = "sbwait" /\ ~\E w \in queue : w.what = "sb"
    /\ queue' = queue \cup
         {[what |-> "j", pos |-> 0, fua |-> FALSE, val |-> Blk("desc", mnt, seq)]}
         \cup {[what |-> "j", pos |-> i, fua |-> FALSE, val |-> Blk("data", mnt, seq)] : i \in Data}
         \cup (IF ASYNC THEN {[what |-> "j", pos |-> CPos, fua |-> FALSE,
                                val |-> Blk("commit", mnt, seq)]} ELSE {})
    /\ phase' = "data"
    /\ UNCHANGED <<sbM, sbC, jM, jC, mnt, seq, acked, bad, why>>

\* sync: data done -> commit block with PREFLUSH|FUA (flush now, then FUA write)
SyncCommit ==
    /\ ~ASYNC /\ phase = "data" /\ queue = {}
    /\ FlushAll(jM, jC, sbM, sbC)
    /\ sbC' = Nil
    /\ queue' = {[what |-> "j", pos |-> CPos, fua |-> TRUE, val |-> Blk("commit", mnt, seq)]}
    /\ phase' = "commitwait"
    /\ UNCHANGED <<mnt, seq, acked, bad, why>>

SyncDone ==
    /\ ~ASYNC /\ phase = "commitwait" /\ queue = {}
    /\ acked' = acked \cup {mnt} /\ phase' = "idle"
    /\ UNCHANGED <<sbM, sbC, jM, jC, queue, mnt, seq, bad, why>>

\* async: everything (incl. commit) done -> blkdev_issue_flush -> acked
AsyncFlush ==
    /\ ASYNC /\ phase = "data" /\ queue = {}
    /\ FlushAll(jM, jC, sbM, sbC)
    /\ sbC' = Nil
    /\ acked' = acked \cup {mnt} /\ phase' = "idle"
    /\ UNCHANGED <<queue, mnt, seq, bad, why>>

Crash ==
    /\ phase \notin {"down"}
    /\ queue' = {} /\ jC' = [p \in Pos |-> Nil] /\ sbC' = Nil
    /\ phase' = "down"
    /\ UNCHANGED <<sbM, jM, mnt, seq, acked, bad, why>>

Next ==
    \/ Mount \/ WriteSb \/ SbDone \/ SyncCommit \/ SyncDone \/ AsyncFlush \/ Crash
    \/ DestageSb \/ \E p \in Pos : DestageJ(p)
    \/ \E w \in queue : Complete(w)

Spec == Init /\ [][Next]_vars

NoBadRecovery == ~bad
=============================================================================
