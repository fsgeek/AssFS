------------------------------ MODULE UndoZero ------------------------------
(***************************************************************************)
(* Episode's undo-zero (logbuf/buffer.c elbb_UndoZero, log.c "force"        *)
(* comment wam 920805, vnops/efs_newalloc.c).                               *)
(*                                                                          *)
(* Extending a file allocates block b, which still holds the previous      *)
(* owner's data ("detritus"). Rather than synchronously zero-filling b:     *)
(*   txn A: map[b] free -> F, then END (bounded; commits promptly)          *)
(*   txn Z: OFILL(b) -- undo means "zero b"; ENDs once the user's data      *)
(*          write to b is done.                                             *)
(* If Z is uncommitted at recovery, b is zeroed, so detritus never shows.   *)
(*                                                                          *)
(* Fixed log order (the script):                                            *)
(*   OFILLFIRST: 1 A.upd  2 Z.ofill  3 A.end  4 Z.end                       *)
(*   otherwise:  1 A.upd  2 A.end    3 Z.ofill 4 Z.end                      *)
(* Knobs: VOLATILE (device write cache), FIX (Z ends only when the data     *)
(* write is covered by a flush, not merely completed).                      *)
(***************************************************************************)
EXTENDS Naturals, Sequences, FiniteSets

CONSTANTS VOLATILE, FIX, OFILLFIRST, Nil

LSNs   == 1..4
Max(S) == IF S = {} THEN 0 ELSE CHOOSE x \in S : \A y \in S : x >= y
StableFrom(S) == Max({k \in 0..4 : \A i \in 1..k : i \in S})

R_Aupd  == [txn |-> "A", kind |-> "upd"]
R_Aend  == [txn |-> "A", kind |-> "end"]
R_Zfill == [txn |-> "Z", kind |-> "ofill"]
R_Zend  == [txn |-> "Z", kind |-> "end"]
Script  == IF OFILLFIRST THEN <<R_Aupd, R_Zfill, R_Aend>>
                         ELSE <<R_Aupd, R_Aend, R_Zfill>>

VARIABLES log, logSubmitted, hostDone, logStable,
          dataSubmitted, dataDone, dataDur, flushSnap,
          queue, clog, dlog, cblk, dblk, flushLBAs

vars == <<log, logSubmitted, hostDone, logStable, dataSubmitted, dataDone,
          dataDur, flushSnap, queue, clog, dlog, cblk, dblk, flushLBAs>>

Init ==
    /\ log = <<>> /\ logSubmitted = {} /\ hostDone = {} /\ logStable = 0
    /\ dataSubmitted = FALSE /\ dataDone = FALSE /\ dataDur = FALSE
    /\ flushSnap = Nil /\ queue = {}
    /\ clog = [n \in LSNs |-> Nil] /\ dlog = [n \in LSNs |-> Nil]
    /\ cblk = Nil /\ dblk = "detritus" /\ flushLBAs = Nil

HasFill == \E i \in 1..Len(log) : log[i].kind = "ofill"

Append1 ==
    /\ Len(log) < 3
    /\ log' = Append(log, Script[Len(log) + 1])
    /\ UNCHANGED <<logSubmitted, hostDone, logStable, dataSubmitted, dataDone,
                   dataDur, flushSnap, queue, clog, dlog, cblk, dblk, flushLBAs>>

\* The user's data write to b, issued once b is allocated and covered.
SubmitData ==
    /\ HasFill /\ ~dataSubmitted
    /\ dataSubmitted' = TRUE
    /\ queue' = queue \cup {[kind |-> "data", id |-> 0, val |-> "U"]}
    /\ UNCHANGED <<log, logSubmitted, hostDone, logStable, dataDone, dataDur,
                   flushSnap, clog, dlog, cblk, dblk, flushLBAs>>

ZEnd ==
    /\ Len(log) = 3
    /\ IF FIX THEN dataDur ELSE dataDone
    /\ log' = Append(log, R_Zend)
    /\ UNCHANGED <<logSubmitted, hostDone, logStable, dataSubmitted, dataDone,
                   dataDur, flushSnap, queue, clog, dlog, cblk, dblk, flushLBAs>>

SubmitLog(n) ==
    /\ n \in 1..Len(log) /\ n \notin logSubmitted
    /\ logSubmitted' = logSubmitted \cup {n}
    /\ queue' = queue \cup {[kind |-> "log", id |-> n, val |-> log[n]]}
    /\ UNCHANGED <<log, hostDone, logStable, dataSubmitted, dataDone, dataDur,
                   flushSnap, clog, dlog, cblk, dblk, flushLBAs>>

Complete(w) ==
    /\ w \in queue
    /\ queue' = queue \ {w}
    /\ IF w.kind = "log"
         THEN /\ hostDone' = hostDone \cup {w.id}
              /\ IF VOLATILE THEN clog' = [clog EXCEPT ![w.id] = w.val] /\ UNCHANGED dlog
                             ELSE dlog' = [dlog EXCEPT ![w.id] = w.val] /\ UNCHANGED clog
              /\ UNCHANGED <<dataDone, cblk, dblk>>
         ELSE /\ dataDone' = TRUE
              /\ IF VOLATILE THEN cblk' = w.val /\ UNCHANGED dblk
                             ELSE dblk' = w.val /\ UNCHANGED cblk
              /\ UNCHANGED <<hostDone, clog, dlog>>
    /\ UNCHANGED <<log, logSubmitted, logStable, dataSubmitted, dataDur,
                   flushSnap, flushLBAs>>

DestageLog(n) ==
    /\ clog[n] # Nil /\ dlog[n] # clog[n]
    /\ dlog' = [dlog EXCEPT ![n] = clog[n]]
    /\ UNCHANGED <<log, logSubmitted, hostDone, logStable, dataSubmitted,
                   dataDone, dataDur, flushSnap, queue, clog, cblk, dblk, flushLBAs>>

DestageBlk ==
    /\ cblk # Nil /\ dblk # cblk
    /\ dblk' = cblk
    /\ UNCHANGED <<log, logSubmitted, hostDone, logStable, dataSubmitted,
                   dataDone, dataDur, flushSnap, queue, clog, dlog, cblk, flushLBAs>>

\* Without a volatile cache there is nothing to flush; durability comes
\* from completion, so the FIX knob reduces to dataDone.
FlushIssue ==
    /\ flushSnap = Nil
    /\ flushSnap' = [log |-> hostDone, data |-> dataDone]
    /\ flushLBAs' = [logs |-> {n \in LSNs : clog[n] # Nil}, blk |-> cblk # Nil]
    /\ UNCHANGED <<log, logSubmitted, hostDone, logStable, dataSubmitted,
                   dataDone, dataDur, queue, clog, dlog, cblk, dblk>>

FlushDone ==
    /\ flushSnap # Nil
    /\ dlog' = [n \in LSNs |-> IF n \in flushLBAs.logs THEN clog[n] ELSE dlog[n]]
    /\ dblk' = IF flushLBAs.blk THEN cblk ELSE dblk
    /\ logStable' = Max({logStable, StableFrom(flushSnap.log)})
    /\ dataDur' = (dataDur \/ flushSnap.data)
    /\ flushSnap' = Nil /\ flushLBAs' = Nil
    /\ UNCHANGED <<log, logSubmitted, hostDone, dataSubmitted, dataDone,
                   queue, clog, cblk>>

Next ==
    \/ Append1 \/ SubmitData \/ ZEnd \/ DestageBlk \/ FlushIssue \/ FlushDone
    \/ \E n \in LSNs : SubmitLog(n) \/ DestageLog(n)
    \/ \E w \in queue : Complete(w)

Spec == Init /\ [][Next]_vars

(* ---------------- recovery from media ---------------- *)
ValidLen  == Max({k \in 0..4 : \A i \in 1..k : dlog[i] # Nil})
InPrefix(r) == \E i \in 1..ValidLen : dlog[i] = r
Committed == {t \in {"A", "Z"} : InPrefix([txn |-> t, kind |-> "end"])}

RMap == IF InPrefix(R_Aupd) /\ "A" \in Committed THEN "F" ELSE "free"
RBlk == IF InPrefix(R_Zfill) /\ "Z" \notin Committed THEN "zero" ELSE dblk

\* A block that recovery says belongs to the file never shows detritus.
NoExposure == RMap = "F" => RBlk \in {"zero", "U"}
=============================================================================
