----------------------------- MODULE LostWrite -----------------------------
(***************************************************************************)
(* StealNoForce (WAL + flush snapshot, both correct) extended with:         *)
(*   - checkpoint / log truncation, recorded in a header block;             *)
(*   - a lying device: a page write may "complete" without being written    *)
(*     (lost write), at most MaxFaults times.                               *)
(*                                                                          *)
(* Knobs:                                                                   *)
(*   TRUNC     - the host may truncate the log.                             *)
(*   MaxFaults - number of lost writes the device may commit.               *)
(*   DETECT    - each media page carries the LSN of its contents, the       *)
(*               header records the LSN each page is expected to have       *)
(*               (a parent-held version), and recovery fails stop on a      *)
(*               mismatch.                                                  *)
(***************************************************************************)
EXTENDS Naturals, Sequences, FiniteSets

CONSTANTS Pages, Txns, MaxUpdates, TRUNC, MaxFaults, DETECT, Nil

MaxLSN  == Cardinality(Txns) * (MaxUpdates + 1)
LSNs    == 1..MaxLSN
Init0   == <<Nil, 0>>
Max(S)  == IF S = {} THEN 0 ELSE CHOOSE x \in S : \A y \in S : x >= y
StableFrom(S) == Max({k \in 0..MaxLSN : \A i \in 1..k : i \in S})
NoPage  == [val |-> Init0, lsn |-> 0]
Hdr0    == [t |-> 0, exp |-> [p \in Pages |-> 0]]

VARIABLES
    \* host
    mem, pageLSN, log, owner, status, nupd, logSubmitted,
    hostDone,       \* log LSNs seen complete
    pageDone,       \* [Pages -> LSN of the page version last seen complete]
    logStable,      \* log known durable through here
    pageDur,        \* [Pages -> LSN of the page version known durable]
    hostTrunc,      \* truncation point the host has issued
    flushSnap,      \* Nil or [log |-> hostDone, page |-> pageDone] at issue
    \* device
    queue, clog, cpage, chdr, dlog, dpage, dhdr, flushLBAs, faults

hostVars == <<mem, pageLSN, log, owner, status, nupd, logSubmitted, hostDone,
              pageDone, logStable, pageDur, hostTrunc, flushSnap>>
devVars  == <<queue, clog, cpage, chdr, dlog, dpage, dhdr, flushLBAs, faults>>
vars     == <<hostVars, devVars>>

Init ==
    /\ mem = [p \in Pages |-> Init0]
    /\ pageLSN = [p \in Pages |-> 0]
    /\ log = <<>>
    /\ owner = [p \in Pages |-> Nil]
    /\ status = [t \in Txns |-> "idle"]
    /\ nupd = [t \in Txns |-> 0]
    /\ logSubmitted = {}
    /\ hostDone = {}
    /\ pageDone = [p \in Pages |-> 0]
    /\ logStable = 0
    /\ pageDur = [p \in Pages |-> 0]
    /\ hostTrunc = 0
    /\ flushSnap = Nil
    /\ queue = {}
    /\ clog = [n \in LSNs |-> Nil]
    /\ cpage = [p \in Pages |-> Nil]
    /\ chdr = Nil
    /\ dlog = [n \in LSNs |-> Nil]
    /\ dpage = [p \in Pages |-> NoPage]
    /\ dhdr = Hdr0
    /\ flushLBAs = Nil
    /\ faults = 0

(* ---------------- host: transactions ---------------- *)
Update(t, p) ==
    /\ status[t] \in {"idle", "active"}
    /\ nupd[t] < MaxUpdates
    /\ owner[p] \in {Nil, t}
    /\ LET l == Len(log) + 1
           v == <<t, nupd[t] + 1>>
       IN /\ log' = Append(log, [lsn |-> l, txn |-> t, kind |-> "upd",
                                 page |-> p, old |-> mem[p], new |-> v])
          /\ mem' = [mem EXCEPT ![p] = v]
          /\ pageLSN' = [pageLSN EXCEPT ![p] = l]
    /\ owner' = [owner EXCEPT ![p] = t]
    /\ status' = [status EXCEPT ![t] = "active"]
    /\ nupd' = [nupd EXCEPT ![t] = @ + 1]
    /\ UNCHANGED <<logSubmitted, hostDone, pageDone, logStable, pageDur,
                   hostTrunc, flushSnap, devVars>>

Commit(t) ==
    /\ status[t] = "active"
    /\ log' = Append(log, [lsn |-> Len(log) + 1, txn |-> t, kind |-> "end",
                           page |-> Nil, old |-> Nil, new |-> Nil])
    /\ status' = [status EXCEPT ![t] = "committing"]
    /\ owner' = [p \in Pages |-> IF owner[p] = t THEN Nil ELSE owner[p]]
    /\ UNCHANGED <<mem, pageLSN, nupd, logSubmitted, hostDone, pageDone,
                   logStable, pageDur, hostTrunc, flushSnap, devVars>>

EndLSN(t) == CHOOSE i \in 1..Len(log) : log[i].kind = "end" /\ log[i].txn = t

Ack(t) ==
    /\ status[t] = "committing"
    /\ EndLSN(t) <= logStable
    /\ status' = [status EXCEPT ![t] = "acked"]
    /\ UNCHANGED <<mem, pageLSN, log, owner, nupd, logSubmitted, hostDone,
                   pageDone, logStable, pageDur, hostTrunc, flushSnap, devVars>>

(* ---------------- host: I/O ---------------- *)
SubmitLog(n) ==
    /\ n \in 1..Len(log) /\ n \notin logSubmitted
    /\ logSubmitted' = logSubmitted \cup {n}
    /\ queue' = queue \cup {[kind |-> "log", id |-> n, val |-> log[n]]}
    /\ UNCHANGED <<mem, pageLSN, log, owner, status, nupd, hostDone, pageDone,
                   logStable, pageDur, hostTrunc, flushSnap,
                   clog, cpage, chdr, dlog, dpage, dhdr, flushLBAs, faults>>

InFlight(k, i) == \E w \in queue : w.kind = k /\ w.id = i

SubmitPage(p) ==
    /\ ~InFlight("page", p)
    /\ pageLSN[p] > pageDone[p]                 \* dirty w.r.t. what we wrote
    /\ pageLSN[p] <= logStable                  \* WAL
    /\ queue' = queue \cup {[kind |-> "page", id |-> p,
                             val |-> [val |-> mem[p], lsn |-> pageLSN[p]]]}
    /\ UNCHANGED <<hostVars, clog, cpage, chdr, dlog, dpage, dhdr, flushLBAs, faults>>

LastUpd(p, T) == Max({i \in 1..T : log[i].kind = "upd" /\ log[i].page = p})
TxnsUpTo(T)   == {log[i].txn : i \in 1..T}

\* Truncate the log through T. Safe only if every record <= T is no longer
\* needed: its transaction has ended at or before T (no undo needed), and
\* every page's last update <= T is known durable (no redo needed).
TruncOK(T) ==
    /\ T > hostTrunc /\ T <= logStable
    /\ \A t \in TxnsUpTo(T) : \E i \in 1..T : log[i].kind = "end" /\ log[i].txn = t
    /\ \A p \in Pages : LastUpd(p, T) <= pageDur[p]

SubmitHdr(T) ==
    /\ TRUNC
    /\ T \in 1..Len(log)
    /\ ~InFlight("hdr", "hdr")
    /\ TruncOK(T)
    /\ hostTrunc' = T
    /\ queue' = queue \cup {[kind |-> "hdr", id |-> "hdr",
                             val |-> [t |-> T, exp |-> pageDur]]}
    /\ UNCHANGED <<mem, pageLSN, log, owner, status, nupd, logSubmitted,
                   hostDone, pageDone, logStable, pageDur, flushSnap,
                   clog, cpage, chdr, dlog, dpage, dhdr, flushLBAs, faults>>

FlushIssue ==
    /\ flushSnap = Nil
    /\ flushSnap' = [log |-> hostDone, page |-> pageDone]
    /\ flushLBAs' = [logs  |-> {n \in LSNs : clog[n] # Nil},
                     pages |-> {p \in Pages : cpage[p] # Nil},
                     hdr   |-> chdr # Nil]
    /\ UNCHANGED <<mem, pageLSN, log, owner, status, nupd, logSubmitted,
                   hostDone, pageDone, logStable, pageDur, hostTrunc,
                   queue, clog, cpage, chdr, dlog, dpage, dhdr, faults>>

FlushDone ==
    /\ flushSnap # Nil
    /\ dlog'  = [n \in LSNs  |-> IF n \in flushLBAs.logs  THEN clog[n]  ELSE dlog[n]]
    /\ dpage' = [p \in Pages |-> IF p \in flushLBAs.pages THEN cpage[p] ELSE dpage[p]]
    /\ dhdr'  = IF flushLBAs.hdr THEN chdr ELSE dhdr
    /\ logStable' = Max({logStable, StableFrom(flushSnap.log)})
    /\ pageDur' = [p \in Pages |-> Max({pageDur[p], flushSnap.page[p]})]
    /\ flushSnap' = Nil
    /\ flushLBAs' = Nil
    /\ UNCHANGED <<mem, pageLSN, log, owner, status, nupd, logSubmitted,
                   hostDone, pageDone, hostTrunc, queue, clog, cpage, chdr, faults>>

(* ---------------- device ---------------- *)
Complete(w) ==
    /\ w \in queue
    /\ queue' = queue \ {w}
    /\ CASE w.kind = "log" ->
              /\ clog' = [clog EXCEPT ![w.id] = w.val]
              /\ hostDone' = hostDone \cup {w.id}
              /\ UNCHANGED <<cpage, chdr, pageDone>>
         [] w.kind = "page" ->
              /\ cpage' = [cpage EXCEPT ![w.id] = w.val]
              /\ pageDone' = [pageDone EXCEPT ![w.id] = w.val.lsn]
              /\ UNCHANGED <<clog, chdr, hostDone>>
         [] w.kind = "hdr" ->
              /\ chdr' = w.val
              /\ UNCHANGED <<clog, cpage, hostDone, pageDone>>
    /\ UNCHANGED <<mem, pageLSN, log, owner, status, nupd, logSubmitted,
                   logStable, pageDur, hostTrunc, flushSnap,
                   dlog, dpage, dhdr, flushLBAs, faults>>

\* The lie: the drive reports a page write complete but never stores it.
LoseWrite(w) ==
    /\ w \in queue /\ w.kind = "page" /\ faults < MaxFaults
    /\ queue' = queue \ {w}
    /\ pageDone' = [pageDone EXCEPT ![w.id] = w.val.lsn]
    /\ faults' = faults + 1
    /\ UNCHANGED <<mem, pageLSN, log, owner, status, nupd, logSubmitted,
                   hostDone, logStable, pageDur, hostTrunc, flushSnap,
                   clog, cpage, chdr, dlog, dpage, dhdr, flushLBAs>>

DestageLog(n) ==
    /\ clog[n] # Nil /\ dlog[n] # clog[n]
    /\ dlog' = [dlog EXCEPT ![n] = clog[n]]
    /\ UNCHANGED <<hostVars, queue, clog, cpage, chdr, dpage, dhdr, flushLBAs, faults>>

DestagePage(p) ==
    /\ cpage[p] # Nil /\ dpage[p] # cpage[p]
    /\ dpage' = [dpage EXCEPT ![p] = cpage[p]]
    /\ UNCHANGED <<hostVars, queue, clog, cpage, chdr, dlog, dhdr, flushLBAs, faults>>

DestageHdr ==
    /\ chdr # Nil /\ dhdr # chdr
    /\ dhdr' = chdr
    /\ UNCHANGED <<hostVars, queue, clog, cpage, chdr, dlog, dpage, flushLBAs, faults>>

Next ==
    \/ \E t \in Txns, p \in Pages : Update(t, p)
    \/ \E t \in Txns : Commit(t) \/ Ack(t)
    \/ \E n \in LSNs : SubmitLog(n) \/ DestageLog(n) \/ SubmitHdr(n)
    \/ \E p \in Pages : SubmitPage(p) \/ DestagePage(p)
    \/ DestageHdr \/ FlushIssue \/ FlushDone
    \/ \E w \in queue : Complete(w) \/ LoseWrite(w)

Spec == Init /\ [][Next]_vars

(* ---------------- recovery from media ---------------- *)
Start    == dhdr.t
ValidEnd == Max({k \in Start..MaxLSN : \A i \in (Start + 1)..k : dlog[i] # Nil})

\* Transactions that ended before the truncation point are committed by
\* construction (TruncOK); the rest need an END in the scanned log.
Committed ==
    {log[i].txn : i \in {j \in 1..Start : log[j].kind = "end"}}
    \cup {dlog[i].txn : i \in {j \in (Start + 1)..ValidEnd : dlog[j].kind = "end"}}

Salvage == DETECT /\ \E p \in Pages : dpage[p].lsn < dhdr.exp[p]

RECURSIVE Redo(_, _)
Redo(st, i) ==
    IF i > ValidEnd THEN st
    ELSE Redo(IF dlog[i].kind = "upd"
                THEN [st EXCEPT ![dlog[i].page] = dlog[i].new] ELSE st, i + 1)

RECURSIVE Undo(_, _)
Undo(st, i) ==
    IF i <= Start THEN st
    ELSE Undo(IF dlog[i].kind = "upd" /\ dlog[i].txn \notin Committed
                THEN [st EXCEPT ![dlog[i].page] = dlog[i].old] ELSE st, i - 1)

Recovered == Undo(Redo([p \in Pages |-> dpage[p].val], Start + 1), ValidEnd)

RECURSIVE Apply(_, _)
Apply(st, i) ==
    IF i > Len(log) THEN st
    ELSE Apply(IF log[i].kind = "upd" /\ log[i].txn \in Committed
                 THEN [st EXCEPT ![log[i].page] = log[i].new] ELSE st, i + 1)

Expected == Apply([p \in Pages |-> Init0], 1)

NoSalvage  == ~Salvage
Durability == ~Salvage => \A t \in Txns : status[t] = "acked" => t \in Committed
Atomicity  == ~Salvage => Recovered = Expected
=============================================================================
