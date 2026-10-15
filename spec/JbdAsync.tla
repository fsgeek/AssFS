------------------------------ MODULE JbdAsync ------------------------------
(***************************************************************************)
(* Linux jbd2 journal_async_commit with csum v2/v3, several transactions,  *)
(* and a proposed recovery fix.                                             *)
(*                                                                          *)
(* Writer (fs/jbd2/commit.c), one transaction at a time (kjournald2):       *)
(*   async : submit descriptor, data, and commit block, no flush; wait for  *)
(*           all to complete; blkdev_issue_flush; transaction acknowledged. *)
(*   sync  : submit descriptor and data; wait; commit with PREFLUSH|FUA;    *)
(*           acknowledged when it completes.                                *)
(* Device: completion lands in a volatile cache; destage in any order;      *)
(* flush; FUA. CORRUPT allows one media corruption of a data block of an    *)
(* acknowledged transaction.                                                *)
(*                                                                          *)
(* Recovery (fs/jbd2/recovery.c do_one_pass), from a fresh log:             *)
(*   K = number of leading transactions whose descriptor and commit block   *)
(*   are present (valid magic, sequence, self-checksum).                    *)
(*   FIX = FALSE (as written): PASS_REPLAY writes every tagged block of     *)
(*     1..K whose tag checksum matches, skips the rest, returns EFSBADCRC.  *)
(*   FIX = TRUE (proposed): PASS_SCAN also checks the tag checksums when    *)
(*     async_commit is set. Let F be the first transaction in 1..K with a   *)
(*     bad tag. If F = K, end the log before F (an interrupted commit): no  *)
(*     error. If F < K, a later commit exists, so F was flushed and is      *)
(*     corrupt: replay 1..F-1 and fail the mount (j_failed_commit), as the  *)
(*     csum v1 path does today.                                             *)
(***************************************************************************)
EXTENDS Naturals, FiniteSets

CONSTANTS T, B, ASYNC, FIX, CORRUPT

Txns  == 1..T
Data  == 1..B
Pos   == 0..(B + 1)                    \* 0 desc, 1..B data, B+1 commit
CPos  == B + 1
Blocks == Txns \X Pos

VARIABLES
    media,      \* [Blocks -> {"empty","ok","bad"}]
    cache,      \* set of blocks completed into the volatile cache
    queue,      \* set of blocks submitted, not completed (with fua flag)
    cur,        \* transaction being committed (T+1 = done)
    phase,      \* "submit" | "wait" | "cwait"
    acked,      \* acknowledged transactions
    corrupted   \* a corruption has happened

vars == <<media, cache, queue, cur, phase, acked, corrupted>>

Init ==
    /\ media = [b \in Blocks |-> "empty"]
    /\ cache = {} /\ queue = {}
    /\ cur = 1 /\ phase = "submit"
    /\ acked = {} /\ corrupted = FALSE

W(t, p, fua) == [blk |-> <<t, p>>, fua |-> fua]

(* ---------------- device ---------------- *)
Complete(w) ==
    /\ w \in queue
    /\ queue' = queue \ {w}
    /\ IF w.fua THEN media' = [media EXCEPT ![w.blk] = "ok"] /\ UNCHANGED cache
                ELSE cache' = cache \cup {w.blk} /\ UNCHANGED media
    /\ UNCHANGED <<cur, phase, acked, corrupted>>

Destage(b) ==
    /\ b \in cache
    /\ media' = [media EXCEPT ![b] = "ok"] /\ cache' = cache \ {b}
    /\ UNCHANGED <<queue, cur, phase, acked, corrupted>>

Flushed(m) == [b \in Blocks |-> IF b \in cache THEN "ok" ELSE m[b]]

Corrupt(t, i) ==
    /\ CORRUPT /\ ~corrupted /\ t \in acked /\ media[<<t, i>>] = "ok"
    /\ media' = [media EXCEPT ![<<t, i>>] = "bad"] /\ corrupted' = TRUE
    /\ UNCHANGED <<cache, queue, cur, phase, acked>>

(* ---------------- writer ---------------- *)
Submit ==
    /\ cur <= T /\ phase = "submit"
    /\ queue' = {W(cur, p, FALSE) : p \in 0..B}
                \cup (IF ASYNC THEN {W(cur, CPos, FALSE)} ELSE {})
    /\ phase' = "wait"
    /\ UNCHANGED <<media, cache, cur, acked, corrupted>>

\* async: everything completed -> blkdev_issue_flush -> acknowledged
AsyncFlush ==
    /\ ASYNC /\ cur <= T /\ phase = "wait" /\ queue = {}
    /\ media' = Flushed(media) /\ cache' = {}
    /\ acked' = acked \cup {cur} /\ cur' = cur + 1 /\ phase' = "submit"
    /\ UNCHANGED <<queue, corrupted>>

\* sync: data completed -> commit with PREFLUSH|FUA
SyncCommit ==
    /\ ~ASYNC /\ cur <= T /\ phase = "wait" /\ queue = {}
    /\ media' = Flushed(media) /\ cache' = {}
    /\ queue' = {W(cur, CPos, TRUE)}
    /\ phase' = "cwait"
    /\ UNCHANGED <<cur, acked, corrupted>>

SyncDone ==
    /\ ~ASYNC /\ cur <= T /\ phase = "cwait" /\ queue = {}
    /\ acked' = acked \cup {cur} /\ cur' = cur + 1 /\ phase' = "submit"
    /\ UNCHANGED <<media, cache, queue, corrupted>>

Next ==
    \/ Submit \/ AsyncFlush \/ SyncCommit \/ SyncDone
    \/ \E w \in queue : Complete(w)
    \/ \E b \in cache : Destage(b)
    \/ \E t \in Txns, i \in Data : Corrupt(t, i)

Spec == Init /\ [][Next]_vars

(* ---------------- recovery if power failed now (media only) ---------------- *)
Present(t) == media[<<t, 0>>] = "ok" /\ media[<<t, CPos>>] = "ok"
TagOK(t, i) == media[<<t, i>>] = "ok"
TxOK(t) == \A i \in Data : TagOK(t, i)

K == IF \E k \in 0..T : \A t \in 1..k : Present(t)
       THEN CHOOSE k \in 0..T : (\A t \in 1..k : Present(t))
                                /\ (k = T \/ ~Present(k + 1))
       ELSE 0

BadTx == {t \in 1..K : ~TxOK(t)}
F     == IF BadTx = {} THEN K + 1 ELSE CHOOSE f \in BadTx : \A g \in BadTx : f <= g

\* Blocks written to their home locations, and whether the mount fails.
Replayed ==
    IF ~FIX THEN {<<t, i>> \in (1..K) \X Data : TagOK(t, i)}
    ELSE (1..(F - 1)) \X Data
MountFails ==
    IF ~FIX THEN BadTx # {} ELSE F < K

(* ---------------- properties ---------------- *)
\* No transaction is partly replayed.
Atomic == \A t \in Txns :
            ({i \in Data : <<t, i>> \in Replayed} = {}) \/
            ({i \in Data : <<t, i>> \in Replayed} = Data)

\* A mount that succeeds has every acknowledged transaction.
Durable == ~MountFails => \A t \in acked : t \in {x[1] : x \in Replayed}

\* Without media corruption the mount never fails.
NoFalseAlarm == ~corrupted => ~MountFails
=============================================================================
