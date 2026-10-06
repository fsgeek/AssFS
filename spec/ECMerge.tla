------------------------------ MODULE ECMerge ------------------------------
(***************************************************************************)
(* Episode equivalence-class merge (logbuf/tran.c, recover.c), modelled    *)
(* from the code.                                                           *)
(*                                                                          *)
(*  - No page locks. When t writes a page whose last writer u has not       *)
(*    completed, ECMerge(u, t) joins their classes (smaller folds into      *)
(*    larger; the folded head's ecHead points at the new head, so heads     *)
(*    chain; members keep their old pointers).                              *)
(*  - ecSize counts members that have not ended. EndTran decrements the     *)
(*    head's ecSize; if it stays > 0 the caller goes PENDING. The caller    *)
(*    that takes it to 0 writes a BEND record, under the HEAD's tranId,     *)
(*    for each member, then the head's END; all members complete.           *)
(*  - Recovery: a transaction is committed if it has an END, or if a BEND  *)
(*    naming it was written by a head that has an END (HandleBulkEnd).      *)
(*  - No runtime abort. Device is abstracted: any prefix of the log may be  *)
(*    durable; a page may be stolen once the log is durable through its     *)
(*    last update (WAL).                                                    *)
(*                                                                          *)
(* Knobs: MERGE (do the merge at all), BENDOWN (recovery honours a BEND     *)
(* without its head's END), DOUBLE (merge also counts the folded class's    *)
(* pending members: the bug found in AssFS's ec_merge).                     *)
(***************************************************************************)
EXTENDS Naturals, Sequences, FiniteSets

CONSTANTS Pages, Txns, MaxUpdates, MERGE, BENDOWN, DOUBLE, Nil

Init0  == <<Nil, 0>>
Max(S) == IF S = {} THEN 0 ELSE CHOOSE x \in S : \A y \in S : x >= y

VARIABLES mem, pageLSN, owner, log, status, nupd,
          ecHead, ecSize, ecList, dl, dpage

vars == <<mem, pageLSN, owner, log, status, nupd, ecHead, ecSize, ecList, dl, dpage>>

Init ==
    /\ mem = [p \in Pages |-> Init0]
    /\ pageLSN = [p \in Pages |-> 0]
    /\ owner = [p \in Pages |-> Nil]
    /\ log = <<>>
    /\ status = [t \in Txns |-> "idle"]
    /\ nupd = [t \in Txns |-> 0]
    /\ ecHead = [t \in Txns |-> t]
    /\ ecSize = [t \in Txns |-> 0]
    /\ ecList = [t \in Txns |-> {}]
    /\ dl = 0
    /\ dpage = [p \in Pages |-> Init0]

RECURSIVE Root(_)
Root(t) == IF ecHead[t] = t THEN t ELSE Root(ecHead[t])

Begin(t) ==
    /\ status[t] = "idle"
    /\ status' = [status EXCEPT ![t] = "active"]
    /\ ecSize' = [ecSize EXCEPT ![t] = 1]
    /\ UNCHANGED <<mem, pageLSN, owner, log, nupd, ecHead, ecList, dl, dpage>>

\* elbt_ECMerge(tran1 = u, tran2 = t): returns early if u is complete or the
\* two are already equivalent; otherwise folds the smaller EC into the larger.
MergeOK(u, t) ==
    MERGE /\ u # Nil /\ u # t /\ status[u] \in {"active", "pending"}
          /\ Root(u) # Root(t)

Fold(u, t) ==
    LET r1 == Root(u)
        r2 == Root(t)
        e1 == IF ecSize[r1] > ecSize[r2] THEN r2 ELSE r1   \* folded
        e2 == IF ecSize[r1] > ecSize[r2] THEN r1 ELSE r2   \* survives
        pend == Cardinality({m \in ecList[e1] \cup {e1} : status[m] = "pending"})
        add  == ecSize[e1] + (IF DOUBLE THEN pend ELSE 0)
    IN /\ ecSize' = [ecSize EXCEPT ![e2] = @ + add, ![e1] = 0]
       /\ ecHead' = [ecHead EXCEPT ![e1] = e2]
       /\ ecList' = [ecList EXCEPT ![e2] = @ \cup ecList[e1] \cup {e1}, ![e1] = {}]

Write(t, p) ==
    /\ status[t] = "active"
    /\ nupd[t] < MaxUpdates
    /\ LET u == owner[p] IN
       IF MergeOK(u, t) THEN Fold(u, t) ELSE UNCHANGED <<ecSize, ecHead, ecList>>
    /\ LET l == Len(log) + 1
           v == <<t, nupd[t] + 1>>
       IN /\ log' = Append(log, [kind |-> "upd", txn |-> t, page |-> p,
                                 old |-> mem[p], new |-> v, mem |-> Nil])
          /\ mem' = [mem EXCEPT ![p] = v]
          /\ pageLSN' = [pageLSN EXCEPT ![p] = l]
    /\ owner' = [owner EXCEPT ![p] = t]
    /\ nupd' = [nupd EXCEPT ![t] = @ + 1]
    /\ UNCHANGED <<status, dl, dpage>>

RECURSIVE SetToSeq(_)
SetToSeq(S) == IF S = {} THEN <<>>
               ELSE LET x == CHOOSE y \in S : TRUE IN <<x>> \o SetToSeq(S \ {x})

BendRec(h, m) == [kind |-> "bend", txn |-> h, page |-> Nil, old |-> Nil, new |-> Nil, mem |-> m]
EndRec(h)     == [kind |-> "end",  txn |-> h, page |-> Nil, old |-> Nil, new |-> Nil, mem |-> Nil]

\* elbt_EndTran. The last ender writes BENDs then the head's END in one
\* step; each record still becomes durable separately (dl advances by one),
\* so a crash between them is explored.
End(t) ==
    /\ status[t] = "active"
    /\ nupd[t] > 0              \* every transaction writes something
    /\ LET h == Root(t) IN
       IF ecSize[h] > 1
         THEN /\ ecSize' = [ecSize EXCEPT ![h] = @ - 1]
              /\ status' = [status EXCEPT ![t] = "pending"]
              /\ UNCHANGED <<log, ecList>>
         ELSE LET members == ecList[h]
                  bends   == [i \in 1..Cardinality(members) |->
                                BendRec(h, SetToSeq(members)[i])]
              IN /\ log' = log \o bends \o <<EndRec(h)>>
                 /\ ecSize' = [ecSize EXCEPT ![h] = 0]
                 /\ ecList' = [ecList EXCEPT ![h] = {}]
                 /\ status' = [x \in Txns |->
                                 IF x \in members \cup {h, t} THEN "complete"
                                 ELSE status[x]]
    /\ UNCHANGED <<mem, pageLSN, owner, nupd, ecHead, dl, dpage>>

LogDurable ==
    /\ dl < Len(log)
    /\ dl' = dl + 1
    /\ UNCHANGED <<mem, pageLSN, owner, log, status, nupd, ecHead, ecSize, ecList, dpage>>

Steal(p) ==
    /\ dpage[p] # mem[p]
    /\ pageLSN[p] <= dl
    /\ dpage' = [dpage EXCEPT ![p] = mem[p]]
    /\ UNCHANGED <<mem, pageLSN, owner, log, status, nupd, ecHead, ecSize, ecList, dl>>

Next ==
    \/ \E t \in Txns : Begin(t) \/ End(t)
    \/ \E t \in Txns, p \in Pages : Write(t, p)
    \/ LogDurable
    \/ \E p \in Pages : Steal(p)

Spec == Init /\ [][Next]_vars

(* ---------------- recovery from the durable prefix ---------------- *)
DLog  == SubSeq(log, 1, dl)
Heads == {DLog[i].txn : i \in {j \in 1..dl : DLog[j].kind = "end"}}
Bent  == {DLog[i].mem : i \in {j \in 1..dl : DLog[j].kind = "bend"
                                            /\ (BENDOWN \/ DLog[j].txn \in Heads)}}
Committed == Heads \cup Bent

RECURSIVE Redo(_, _)
Redo(st, i) == IF i > dl THEN st
               ELSE Redo(IF DLog[i].kind = "upd"
                           THEN [st EXCEPT ![DLog[i].page] = DLog[i].new] ELSE st, i + 1)
RECURSIVE Undo(_, _)
Undo(st, i) == IF i = 0 THEN st
               ELSE Undo(IF DLog[i].kind = "upd" /\ DLog[i].txn \notin Committed
                           THEN [st EXCEPT ![DLog[i].page] = DLog[i].old] ELSE st, i - 1)
Recovered == Undo(Redo(dpage, 1), dl)

RECURSIVE Apply(_, _)
Apply(st, i) == IF i > Len(log) THEN st
                ELSE Apply(IF log[i].kind = "upd" /\ log[i].txn \in Committed
                             THEN [st EXCEPT ![log[i].page] = log[i].new] ELSE st, i + 1)
Expected == Apply([p \in Pages |-> Init0], 1)

Atomicity == Recovered = Expected

\* Liveness as a safety check: a pending transaction whose class has no
\* active member left can never complete.
Members(h) == ecList[h] \cup {h}
NoStuck == \A t \in Txns :
             status[t] = "pending" => \E m \in Members(Root(t)) : status[m] = "active"

\* Consistency point: immediately after a commit (log ends in END), every
\* transaction that has written anything is complete -- the committed
\* state is the whole in-memory state, as at a checkpoint.
ConsistencyPoint ==
    (Len(log) > 0 /\ log[Len(log)].kind = "end")
      => \A t \in Txns : nupd[t] > 0 => status[t] = "complete"
=============================================================================
