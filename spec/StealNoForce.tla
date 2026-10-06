---------------------------- MODULE StealNoForce ----------------------------
(***************************************************************************)
(* Episode-style old/new value logging, STEAL / NO-FORCE, over an NVMe     *)
(* device crash model.                                                      *)
(*                                                                          *)
(* Device model (NVMe with a volatile write cache):                        *)
(*   - submitted writes complete in any order;                              *)
(*   - a completed write sits in the volatile cache (per-LBA, latest wins); *)
(*   - the drive may destage any cached LBA to media at any moment;         *)
(*   - Flush guarantees durability only for writes completed BEFORE the     *)
(*     flush was issued.                                                    *)
(* A crash loses the queue and the cache. Because destage is arbitrary,     *)
(* the media state in every reachable state ranges over all possible        *)
(* post-crash states, so we never model the crash itself: the invariant     *)
(* asks "if power failed now, would recovery be right?" in every state.     *)
(*                                                                          *)
(* Host protocol knobs:                                                     *)
(*   WAL  - a dirty page may be submitted only if the log is known durable  *)
(*          through that page's LSN.                                        *)
(*   SNAP - logStable is advanced only from log writes the host saw         *)
(*          complete BEFORE issuing the flush (vs. at flush completion).    *)
(***************************************************************************)
EXTENDS Naturals, Sequences, FiniteSets

CONSTANTS Pages, Txns, MaxUpdates, WAL, SNAP, Nil

MaxLSN  == Cardinality(Txns) * (MaxUpdates + 1)
LSNs    == 1..MaxLSN
Init0   == <<Nil, 0>>   \* a tuple, so TLC can compare it with <<t, k>>
Values  == (Txns \X (1..MaxUpdates)) \cup {Init0}
Max(S)  == IF S = {} THEN 0 ELSE CHOOSE x \in S : \A y \in S : x >= y

VARIABLES
    \* ---- host memory ----
    mem,        \* [Pages -> Values]      buffer cache contents
    pageLSN,    \* [Pages -> 0..MaxLSN]   LSN of last update to the buffer
    log,        \* Seq(record)            in-memory log; index = LSN
    owner,      \* [Pages -> Txns \cup {Nil}]  page lock holder
    status,     \* [Txns -> {"idle","active","committing","acked"}]
    nupd,       \* [Txns -> 0..MaxUpdates]
    logSubmitted, \* set of LSNs already submitted to the device
    hostDone,   \* LSNs whose log write the host has seen complete
    logStable,  \* host's belief: log durable through this LSN
    flushSnap,  \* Nil, or the host-visible completed-LSN set at flush issue
    \* ---- device ----
    queue,      \* set of submitted, uncompleted writes
    clog, cpage,  \* volatile cache  (Nil = nothing cached)
    dlog, dpage,  \* media
    flushLBAs   \* Nil, or the cache contents a pending flush must persist

hostVars   == <<mem, pageLSN, log, owner, status, nupd, logSubmitted,
               hostDone, logStable, flushSnap>>
deviceVars == <<queue, clog, cpage, dlog, dpage, flushLBAs>>
vars       == <<hostVars, deviceVars>>

UpdRec(l, t, p, o, n) == [lsn |-> l, txn |-> t, kind |-> "upd", page |-> p, old |-> o, new |-> n]
EndRec(l, t)          == [lsn |-> l, txn |-> t, kind |-> "end", page |-> Nil, old |-> Nil, new |-> Nil]

Init ==
    /\ mem = [p \in Pages |-> Init0]
    /\ pageLSN = [p \in Pages |-> 0]
    /\ log = <<>>
    /\ owner = [p \in Pages |-> Nil]
    /\ status = [t \in Txns |-> "idle"]
    /\ nupd = [t \in Txns |-> 0]
    /\ logSubmitted = {}
    /\ hostDone = {}
    /\ logStable = 0
    /\ flushSnap = Nil
    /\ queue = {}
    /\ clog = [n \in LSNs |-> Nil]
    /\ cpage = [p \in Pages |-> Nil]
    /\ dlog = [n \in LSNs |-> Nil]
    /\ dpage = [p \in Pages |-> Init0]
    /\ flushLBAs = Nil

(***************************************************************************)
(* Host: transactions                                                       *)
(***************************************************************************)
Update(t, p) ==
    /\ status[t] \in {"idle", "active"}
    /\ nupd[t] < MaxUpdates
    /\ owner[p] \in {Nil, t}
    /\ LET l == Len(log) + 1
           v == <<t, nupd[t] + 1>>
       IN /\ log' = Append(log, UpdRec(l, t, p, mem[p], v))
          /\ mem' = [mem EXCEPT ![p] = v]
          /\ pageLSN' = [pageLSN EXCEPT ![p] = l]
    /\ owner' = [owner EXCEPT ![p] = t]
    /\ status' = [status EXCEPT ![t] = "active"]
    /\ nupd' = [nupd EXCEPT ![t] = @ + 1]
    /\ UNCHANGED <<logSubmitted, hostDone, logStable, flushSnap, deviceVars>>

\* Append END and release locks immediately (early lock release):
\* the lock is dropped before END is durable.
Commit(t) ==
    /\ status[t] = "active"
    /\ log' = Append(log, EndRec(Len(log) + 1, t))
    /\ status' = [status EXCEPT ![t] = "committing"]
    /\ owner' = [p \in Pages |-> IF owner[p] = t THEN Nil ELSE owner[p]]
    /\ UNCHANGED <<mem, pageLSN, nupd, logSubmitted, hostDone, logStable,
                   flushSnap, deviceVars>>

EndLSN(t) == CHOOSE i \in 1..Len(log) : log[i].kind = "end" /\ log[i].txn = t

\* Acknowledge commit to the client once END is believed durable.
Ack(t) ==
    /\ status[t] = "committing"
    /\ EndLSN(t) <= logStable
    /\ status' = [status EXCEPT ![t] = "acked"]
    /\ UNCHANGED <<mem, pageLSN, log, owner, nupd, logSubmitted, hostDone,
                   logStable, flushSnap, deviceVars>>

(***************************************************************************)
(* Host: I/O submission                                                     *)
(***************************************************************************)
SubmitLog(n) ==
    /\ n \in 1..Len(log)
    /\ n \notin logSubmitted
    /\ logSubmitted' = logSubmitted \cup {n}
    /\ queue' = queue \cup {[kind |-> "log", id |-> n, val |-> log[n]]}
    /\ UNCHANGED <<mem, pageLSN, log, owner, status, nupd, hostDone,
                   logStable, flushSnap, clog, cpage, dlog, dpage, flushLBAs>>

PageInFlight(p) == \E w \in queue : w.kind = "page" /\ w.id = p

\* STEAL: any dirty page may be written at any time, committed or not.
SubmitPage(p) ==
    /\ ~PageInFlight(p)
    /\ mem[p] # (IF cpage[p] # Nil THEN cpage[p] ELSE dpage[p])
    /\ WAL => pageLSN[p] <= logStable
    /\ queue' = queue \cup {[kind |-> "page", id |-> p, val |-> mem[p]]}
    /\ UNCHANGED <<hostVars, clog, cpage, dlog, dpage, flushLBAs>>

FlushIssue ==
    /\ flushSnap = Nil
    /\ flushSnap' = hostDone
    /\ flushLBAs' = [logs  |-> {n \in LSNs : clog[n] # Nil},
                     pages |-> {p \in Pages : cpage[p] # Nil}]
    /\ UNCHANGED <<mem, pageLSN, log, owner, status, nupd, logSubmitted,
                   hostDone, logStable, queue, clog, cpage, dlog, dpage>>

StableFrom(S) == Max({k \in 0..MaxLSN : \A i \in 1..k : i \in S})

FlushDone ==
    /\ flushSnap # Nil
    /\ dlog'  = [n \in LSNs  |-> IF n \in flushLBAs.logs  THEN clog[n]  ELSE dlog[n]]
    /\ dpage' = [p \in Pages |-> IF p \in flushLBAs.pages THEN cpage[p] ELSE dpage[p]]
    /\ logStable' = Max({logStable,
                         StableFrom(IF SNAP THEN flushSnap ELSE hostDone)})
    /\ flushSnap' = Nil
    /\ flushLBAs' = Nil
    /\ UNCHANGED <<mem, pageLSN, log, owner, status, nupd, logSubmitted,
                   hostDone, queue, clog, cpage>>

(***************************************************************************)
(* Device                                                                   *)
(***************************************************************************)
Complete(w) ==
    /\ w \in queue
    /\ queue' = queue \ {w}
    /\ IF w.kind = "log"
         THEN /\ clog' = [clog EXCEPT ![w.id] = w.val]
              /\ hostDone' = hostDone \cup {w.id}
              /\ UNCHANGED cpage
         ELSE /\ cpage' = [cpage EXCEPT ![w.id] = w.val]
              /\ UNCHANGED <<clog, hostDone>>
    /\ UNCHANGED <<mem, pageLSN, log, owner, status, nupd, logSubmitted,
                   logStable, flushSnap, dlog, dpage, flushLBAs>>

DestageLog(n) ==
    /\ clog[n] # Nil /\ dlog[n] # clog[n]
    /\ dlog' = [dlog EXCEPT ![n] = clog[n]]
    /\ UNCHANGED <<hostVars, queue, clog, cpage, dpage, flushLBAs>>

DestagePage(p) ==
    /\ cpage[p] # Nil /\ dpage[p] # cpage[p]
    /\ dpage' = [dpage EXCEPT ![p] = cpage[p]]
    /\ UNCHANGED <<hostVars, queue, clog, cpage, dlog, flushLBAs>>

Next ==
    \/ \E t \in Txns, p \in Pages : Update(t, p)
    \/ \E t \in Txns : Commit(t) \/ Ack(t)
    \/ \E n \in LSNs : SubmitLog(n) \/ DestageLog(n)
    \/ \E p \in Pages : SubmitPage(p) \/ DestagePage(p)
    \/ FlushIssue \/ FlushDone
    \/ \E w \in queue : Complete(w)

Spec == Init /\ [][Next]_vars

(***************************************************************************)
(* Recovery, computed from media only.                                      *)
(*   1. Scan the log from LSN 1 to the first hole.                          *)
(*   2. Redo every record in the prefix (repeat history).                   *)
(*   3. Undo records of transactions with no END, newest first.             *)
(***************************************************************************)
ValidLen == Max({k \in 0..MaxLSN : \A i \in 1..k : dlog[i] # Nil})
Committed == {dlog[i].txn : i \in {j \in 1..ValidLen : dlog[j].kind = "end"}}

RECURSIVE Redo(_, _, _)
Redo(st, i, n) ==
    IF i > n THEN st
    ELSE LET r == dlog[i] IN
         Redo(IF r.kind = "upd" THEN [st EXCEPT ![r.page] = r.new] ELSE st, i + 1, n)

RECURSIVE Undo(_, _, _)
Undo(st, i, C) ==
    IF i = 0 THEN st
    ELSE LET r == dlog[i] IN
         Undo(IF r.kind = "upd" /\ r.txn \notin C
                THEN [st EXCEPT ![r.page] = r.old] ELSE st, i - 1, C)

Recovered == Undo(Redo(dpage, 1, ValidLen), ValidLen, Committed)

\* Ground truth: apply exactly the recovered-committed transactions'
\* updates, in log order, to the initial state (uses the in-memory log).
RECURSIVE Apply(_, _, _)
Apply(st, i, C) ==
    IF i > Len(log) THEN st
    ELSE LET r == log[i] IN
         Apply(IF r.kind = "upd" /\ r.txn \in C
                 THEN [st EXCEPT ![r.page] = r.new] ELSE st, i + 1, C)

Expected == Apply([p \in Pages |-> Init0], 1, Committed)

(***************************************************************************)
(* Properties                                                               *)
(***************************************************************************)
\* Every acknowledged commit survives a crash.
Durability == \A t \in Txns : status[t] = "acked" => t \in Committed

\* Recovery yields exactly the committed transactions' effects: no
\* uncommitted value survives, no committed value is lost.
Atomicity == Recovered = Expected

TypeOK ==
    /\ mem \in [Pages -> Values]
    /\ dpage \in [Pages -> Values]
    /\ logStable \in 0..MaxLSN
=============================================================================
