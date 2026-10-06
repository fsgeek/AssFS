----------------------------- MODULE EpisodeLog -----------------------------
(***************************************************************************)
(* Episode's paged log (logbuf/log.c, recover.c, circa 1992-1996) over two  *)
(* device contracts.                                                        *)
(*                                                                          *)
(* Log format as Episode wrote it:                                          *)
(*   - records are packed into log pages; a page is written ONCE, when it   *)
(*     is sealed, and the next record starts a new page (no tail rewrite);  *)
(*   - each page header carries io_window = number of log pages still in   *)
(*     transit when this page was written;                                  *)
(*   - per-page checksum: a torn or unwritten page is simply invalid.       *)
(*                                                                          *)
(* Recovery as SafeDumpLog does it (no wraparound modelled):                *)
(*   - head = newest valid page;                                            *)
(*   - walk head's io_window from oldest to newest; at the first invalid    *)
(*     page, pull head back to the page before it;                          *)
(*   - every page from the start through head must be valid, else bail      *)
(*     to the salvager (RCVR_IOERR);                                        *)
(*   - redo all, undo transactions without END.                             *)
(*                                                                          *)
(* Knobs:                                                                   *)
(*   VOLATILE - FALSE: write-through device, completion = on media (the     *)
(*              1992 contract). TRUE: NVMe volatile write cache.            *)
(*   FIX      - FALSE: "stable" means the log page's write completed        *)
(*              (Episode). TRUE: stable means covered by a flush issued     *)
(*              after completion; io_window counts pages not yet stable.    *)
(*   IOW      - recovery honours io_window.                                 *)
(*   DIST     - io_window records the distance back to the oldest in-transit*)
(*              page, instead of the count of in-transit pages (log.c:2990  *)
(*              stores inTransit.length).                                   *)
(***************************************************************************)
EXTENDS Naturals, Sequences, FiniteSets

CONSTANTS Pages, Txns, MaxUpdates, VOLATILE, FIX, IOW, DIST, Nil

MaxRec  == Cardinality(Txns) * (MaxUpdates + 1)
MaxPg   == MaxRec
PgIdx   == 1..MaxPg
Init0   == <<Nil, 0>>
Max(S)  == IF S = {} THEN 0 ELSE CHOOSE x \in S : \A y \in S : x >= y
StableFrom(S) == Max({k \in 0..MaxPg : \A i \in 1..k : i \in S})

VARIABLES
    mem, pageLog,   \* buffer contents; log page holding the buffer's last update
    log,            \* Seq of all records appended (each carries its log page lp)
    cur,            \* number of the open (unsealed) log page
    owner, status, nupd,
    hostDone,       \* log pages whose write the host saw complete
    stablePg,       \* host belief: log pages 1..stablePg are on media
    flushSnap,      \* FIX only: Nil or hostDone at flush issue
    queue,          \* submitted, uncompleted writes
    clp, cpage,     \* volatile cache (only used when VOLATILE)
    dlp, dpage,     \* media
    flushLBAs

vars == <<mem, pageLog, log, cur, owner, status, nupd, hostDone, stablePg,
          flushSnap, queue, clp, cpage, dlp, dpage, flushLBAs>>

Init ==
    /\ mem = [p \in Pages |-> Init0]
    /\ pageLog = [p \in Pages |-> 0]
    /\ log = <<>>
    /\ cur = 1
    /\ owner = [p \in Pages |-> Nil]
    /\ status = [t \in Txns |-> "idle"]
    /\ nupd = [t \in Txns |-> 0]
    /\ hostDone = {}
    /\ stablePg = 0
    /\ flushSnap = Nil
    /\ queue = {}
    /\ clp = [i \in PgIdx |-> Nil]
    /\ cpage = [p \in Pages |-> Nil]
    /\ dlp = [i \in PgIdx |-> Nil]
    /\ dpage = [p \in Pages |-> Init0]
    /\ flushLBAs = Nil

Rec(t, k, p, o, n) == [txn |-> t, kind |-> k, page |-> p, old |-> o, new |-> n, lp |-> cur]

Update(t, p) ==
    /\ status[t] \in {"idle", "active"}
    /\ nupd[t] < MaxUpdates
    /\ owner[p] \in {Nil, t}
    /\ LET v == <<t, nupd[t] + 1>> IN
       /\ log' = Append(log, Rec(t, "upd", p, mem[p], v))
       /\ mem' = [mem EXCEPT ![p] = v]
    /\ pageLog' = [pageLog EXCEPT ![p] = cur]
    /\ owner' = [owner EXCEPT ![p] = t]
    /\ status' = [status EXCEPT ![t] = "active"]
    /\ nupd' = [nupd EXCEPT ![t] = @ + 1]
    /\ UNCHANGED <<cur, hostDone, stablePg, flushSnap, queue, clp, cpage,
                   dlp, dpage, flushLBAs>>

Commit(t) ==
    /\ status[t] = "active"
    /\ log' = Append(log, Rec(t, "end", Nil, Nil, Nil))
    /\ status' = [status EXCEPT ![t] = "committing"]
    /\ owner' = [p \in Pages |-> IF owner[p] = t THEN Nil ELSE owner[p]]
    /\ UNCHANGED <<mem, pageLog, cur, nupd, hostDone, stablePg, flushSnap,
                   queue, clp, cpage, dlp, dpage, flushLBAs>>

EndPg(t) == (CHOOSE i \in 1..Len(log) : log[i].kind = "end" /\ log[i].txn = t)

Ack(t) ==
    /\ status[t] = "committing"
    /\ log[EndPg(t)].lp <= stablePg
    /\ status' = [status EXCEPT ![t] = "acked"]
    /\ UNCHANGED <<mem, pageLog, log, cur, owner, nupd, hostDone, stablePg,
                   flushSnap, queue, clp, cpage, dlp, dpage, flushLBAs>>

InTransitIds == {w.id : w \in {x \in queue : x.kind = "log"}}
InTransit == IF DIST
               THEN IF InTransitIds = {} THEN 0
                    ELSE cur - (CHOOSE i \in InTransitIds : \A j \in InTransitIds : i <= j)
               ELSE Cardinality(InTransitIds)

\* Seal the open page and write it; the next record goes to a fresh page.
Seal ==
    /\ cur <= MaxPg
    /\ \E i \in 1..Len(log) : log[i].lp = cur
    /\ LET recs == SelectSeq(log, LAMBDA r : r.lp = cur)
           iow  == IF FIX THEN (cur - 1) - stablePg ELSE InTransit
       IN queue' = queue \cup {[kind |-> "log", id |-> cur,
                                val |-> [recs |-> recs, iow |-> iow]]}
    /\ cur' = cur + 1
    /\ UNCHANGED <<mem, pageLog, log, owner, status, nupd, hostDone, stablePg,
                   flushSnap, clp, cpage, dlp, dpage, flushLBAs>>

PageInFlight(p) == \E w \in queue : w.kind = "buf" /\ w.id = p
OnDevice(p) == IF VOLATILE /\ cpage[p] # Nil THEN cpage[p] ELSE dpage[p]

\* STEAL with the write-ahead rule: buffer may go once its log page is stable.
SubmitBuf(p) ==
    /\ ~PageInFlight(p)
    /\ mem[p] # OnDevice(p)
    /\ pageLog[p] <= stablePg
    /\ queue' = queue \cup {[kind |-> "buf", id |-> p, val |-> mem[p]]}
    /\ UNCHANGED <<mem, pageLog, log, cur, owner, status, nupd, hostDone,
                   stablePg, flushSnap, clp, cpage, dlp, dpage, flushLBAs>>

Complete(w) ==
    /\ w \in queue
    /\ queue' = queue \ {w}
    /\ IF w.kind = "log"
         THEN /\ hostDone' = hostDone \cup {w.id}
              /\ IF VOLATILE
                   THEN /\ clp' = [clp EXCEPT ![w.id] = w.val] /\ UNCHANGED dlp
                   ELSE /\ dlp' = [dlp EXCEPT ![w.id] = w.val] /\ UNCHANGED clp
              /\ stablePg' = IF FIX THEN stablePg ELSE StableFrom(hostDone')
              /\ UNCHANGED <<cpage, dpage>>
         ELSE /\ IF VOLATILE
                   THEN /\ cpage' = [cpage EXCEPT ![w.id] = w.val] /\ UNCHANGED dpage
                   ELSE /\ dpage' = [dpage EXCEPT ![w.id] = w.val] /\ UNCHANGED cpage
              /\ UNCHANGED <<hostDone, stablePg, clp, dlp>>
    /\ UNCHANGED <<mem, pageLog, log, cur, owner, status, nupd, flushSnap,
                   flushLBAs>>

DestageLog(i) ==
    /\ VOLATILE /\ clp[i] # Nil /\ dlp[i] # clp[i]
    /\ dlp' = [dlp EXCEPT ![i] = clp[i]]
    /\ UNCHANGED <<mem, pageLog, log, cur, owner, status, nupd, hostDone,
                   stablePg, flushSnap, queue, clp, cpage, dpage, flushLBAs>>

DestageBuf(p) ==
    /\ VOLATILE /\ cpage[p] # Nil /\ dpage[p] # cpage[p]
    /\ dpage' = [dpage EXCEPT ![p] = cpage[p]]
    /\ UNCHANGED <<mem, pageLog, log, cur, owner, status, nupd, hostDone,
                   stablePg, flushSnap, queue, clp, cpage, dlp, flushLBAs>>

FlushIssue ==
    /\ FIX /\ VOLATILE /\ flushSnap = Nil
    /\ flushSnap' = hostDone
    /\ flushLBAs' = [lps |-> {i \in PgIdx : clp[i] # Nil},
                     bufs |-> {p \in Pages : cpage[p] # Nil}]
    /\ UNCHANGED <<mem, pageLog, log, cur, owner, status, nupd, hostDone,
                   stablePg, queue, clp, cpage, dlp, dpage>>

FlushDone ==
    /\ flushSnap # Nil
    /\ dlp'   = [i \in PgIdx |-> IF i \in flushLBAs.lps  THEN clp[i]   ELSE dlp[i]]
    /\ dpage' = [p \in Pages |-> IF p \in flushLBAs.bufs THEN cpage[p] ELSE dpage[p]]
    /\ stablePg' = Max({stablePg, StableFrom(flushSnap)})
    /\ flushSnap' = Nil
    /\ flushLBAs' = Nil
    /\ UNCHANGED <<mem, pageLog, log, cur, owner, status, nupd, hostDone,
                   queue, clp, cpage>>

\* Stable but sealed-and-unwritten pages are fine; FIX mode also needs the
\* host to be able to force: when stable lags, WAL simply blocks the buffer.
Next ==
    \/ \E t \in Txns, p \in Pages : Update(t, p)
    \/ \E t \in Txns : Commit(t) \/ Ack(t)
    \/ Seal
    \/ \E p \in Pages : SubmitBuf(p) \/ DestageBuf(p)
    \/ \E i \in PgIdx : DestageLog(i)
    \/ \E w \in queue : Complete(w)
    \/ FlushIssue \/ FlushDone

Spec == Init /\ [][Next]_vars

(***************************************************************************)
(* Recovery from media                                                      *)
(***************************************************************************)
LogHead0 == Max({i \in PgIdx : dlp[i] # Nil})
Missing == IF LogHead0 = 0 THEN {}
           ELSE {k \in 0..dlp[LogHead0].iow : LogHead0 - k >= 1 /\ dlp[LogHead0 - k] = Nil}
LogHead == IF IOW /\ Missing # {} THEN LogHead0 - Max(Missing) - 1 ELSE LogHead0

Salvage == \E i \in 1..LogHead : dlp[i] = Nil

RECURSIVE Gather(_, _)
Gather(i, n) == IF i > n THEN <<>> ELSE dlp[i].recs \o Gather(i + 1, n)

RLog == IF Salvage THEN <<>> ELSE Gather(1, LogHead)
Committed == {RLog[i].txn : i \in {j \in 1..Len(RLog) : RLog[j].kind = "end"}}

RECURSIVE Redo(_, _)
Redo(st, i) ==
    IF i > Len(RLog) THEN st
    ELSE Redo(IF RLog[i].kind = "upd"
                THEN [st EXCEPT ![RLog[i].page] = RLog[i].new] ELSE st, i + 1)

RECURSIVE Undo(_, _)
Undo(st, i) ==
    IF i = 0 THEN st
    ELSE Undo(IF RLog[i].kind = "upd" /\ RLog[i].txn \notin Committed
                THEN [st EXCEPT ![RLog[i].page] = RLog[i].old] ELSE st, i - 1)

Recovered == Undo(Redo(dpage, 1), Len(RLog))

RECURSIVE Apply(_, _)
Apply(st, i) ==
    IF i > Len(log) THEN st
    ELSE Apply(IF log[i].kind = "upd" /\ log[i].txn \in Committed
                 THEN [st EXCEPT ![log[i].page] = log[i].new] ELSE st, i + 1)

Expected == Apply([p \in Pages |-> Init0], 1)

\* Fail-stop: recovery refuses and calls for the salvager.
NoSalvage  == ~Salvage
\* Silent failures: recovery claims success but is wrong.
Durability == ~Salvage => \A t \in Txns : status[t] = "acked" => t \in Committed
Atomicity  == ~Salvage => Recovered = Expected
=============================================================================
