------------------------------ MODULE TornPage ------------------------------
(***************************************************************************)
(* Torn page writes vs. two styles of redo.                                 *)
(*                                                                          *)
(* A page is sectors 0..S; sector 0 is the header (holds the page LSN),     *)
(* 1..S hold data. A page write is NOT atomic: each sector reaches media    *)
(* separately, so a crash can leave any mix of old and new sectors.         *)
(*                                                                          *)
(* Each transaction is a single committing log record covering a           *)
(* contiguous range of data sectors (no undo needed; the question is redo). *)
(* WAL: a page write may start only when the log is durable through the     *)
(* page's LSN. Any prefix of the log may be durable.                        *)
(*                                                                          *)
(*   MODE = "phys"  : Episode. Record carries old and new bytes; redo sets  *)
(*                    the range to the new bytes, unconditionally.          *)
(*   MODE = "delta" : physiological. Record says "add 1 to the range";      *)
(*                    redo applies it only if the media page LSN is older.  *)
(***************************************************************************)
EXTENDS Integers, Sequences, FiniteSets

CONSTANTS S, MaxRecs, MODE, ATOMIC   \* ATOMIC: page writes cannot tear

Data    == 1..S
Sectors == 0..S
Ranges  == {<<a, b>> : a \in Data, b \in Data} \cap {r \in Data \X Data : r[1] <= r[2]}
InR(r, i) == r[1] <= i /\ i <= r[2]

VARIABLES mem, log, dl, inflight, media

vars == <<mem, log, dl, inflight, media>>

Blank == [i \in Sectors |-> 0]          \* sector 0 = page LSN, others = data

Init ==
    /\ mem = Blank /\ media = Blank
    /\ log = <<>> /\ dl = 0
    /\ inflight = [i \in Sectors |-> -1]  \* -1: no write in flight for that sector

Apply1(pg, rec) ==
    [i \in Sectors |-> IF i = 0 THEN rec.lsn
                       ELSE IF InR(rec.r, i)
                              THEN (IF MODE = "phys" THEN rec.new ELSE pg[i] + 1)
                              ELSE pg[i]]

Update(r) ==
    /\ Len(log) < MaxRecs
    /\ LET l   == Len(log) + 1
           rec == [lsn |-> l, r |-> r, new |-> l]   \* phys: range := l
       IN /\ log' = Append(log, rec)
          /\ mem' = Apply1(mem, rec)
    /\ UNCHANGED <<dl, inflight, media>>

LogDurable ==
    /\ dl < Len(log)
    /\ dl' = dl + 1
    /\ UNCHANGED <<mem, log, inflight, media>>

NoWrite == \A i \in Sectors : inflight[i] = -1

StartWrite ==
    /\ NoWrite
    /\ mem # media
    /\ mem[0] <= dl                                  \* WAL
    /\ IF ATOMIC THEN /\ media' = mem /\ UNCHANGED inflight
                 ELSE /\ inflight' = mem /\ UNCHANGED media
    /\ UNCHANGED <<mem, log, dl>>

\* One sector of the in-flight write lands. Any order: torn until all land.
Land(i) ==
    /\ inflight[i] # -1
    /\ media' = [media EXCEPT ![i] = inflight[i]]
    /\ inflight' = [inflight EXCEPT ![i] = -1]
    /\ UNCHANGED <<mem, log, dl>>

Next ==
    \/ \E r \in Ranges : Update(r)
    \/ LogDurable \/ StartWrite
    \/ \E i \in Sectors : Land(i)

Spec == Init /\ [][Next]_vars

(* ---------------- recovery ---------------- *)
RECURSIVE Redo(_, _)
Redo(pg, i) ==
    IF i > dl THEN pg
    ELSE LET rec == log[i]
             apply == MODE = "phys" \/ rec.lsn > media[0]   \* LSN test on MEDIA header
         IN Redo(IF apply THEN Apply1(pg, rec) ELSE pg, i + 1)

RECURSIVE Truth(_, _)
Truth(pg, i) == IF i > dl THEN pg ELSE Truth(Apply1(pg, log[i]), i + 1)

\* Compare data sectors only; the header is bookkeeping.
Correct == \A i \in Data : Redo(media, 1)[i] = Truth(Blank, 1)[i]
=============================================================================
