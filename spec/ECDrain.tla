------------------------------ MODULE ECDrain ------------------------------
(***************************************************************************)
(* Can an equivalence class always drain under unbounded arrivals?          *)
(*                                                                          *)
(* Transactions recycle (complete -> idle), so arrivals never stop. Each    *)
(* writes one page once, then ends; merge and ecSize as in ECMerge.tla.     *)
(* Log space: L units; every record (write, BEND, END) costs one unit;      *)
(* units held by a class are released when the class commits (its records *)
(* no longer pin the log tail).                                             *)
(*                                                                          *)
(*   RESERVE = TRUE : Begin reserves the transaction's whole need (one      *)
(*                    write + one END/BEND) up front, as Episode's          *)
(*                    admission control does.                               *)
(*   RESERVE = FALSE: space is taken record by record, when written.        *)
(*                                                                          *)
(* Fairness only on ending and recycling: arrivals are adversarial.         *)
(***************************************************************************)
EXTENDS Naturals, FiniteSets

CONSTANTS Txns, Pages, L, RESERVE, Nil

VARIABLES status, wrote, owner, ecHead, ecSize, ecList, used, reserved

vars == <<status, wrote, owner, ecHead, ecSize, ecList, used, reserved>>

Init ==
    /\ status = [t \in Txns |-> "idle"]
    /\ wrote = [t \in Txns |-> FALSE]
    /\ owner = [p \in Pages |-> Nil]
    /\ ecHead = [t \in Txns |-> t]
    /\ ecSize = [t \in Txns |-> 0]
    /\ ecList = [t \in Txns |-> {}]
    /\ used = 0
    /\ reserved = 0

RECURSIVE Root(_)
Root(t) == IF ecHead[t] = t THEN t ELSE Root(ecHead[t])
Members(h) == ecList[h] \cup {h}

Begin(t) ==
    /\ status[t] = "idle"
    /\ RESERVE => used + reserved + 2 <= L
    /\ reserved' = IF RESERVE THEN reserved + 2 ELSE reserved
    /\ status' = [status EXCEPT ![t] = "active"]
    /\ ecHead' = [ecHead EXCEPT ![t] = t]
    /\ ecSize' = [ecSize EXCEPT ![t] = 1]
    /\ ecList' = [ecList EXCEPT ![t] = {}]
    /\ UNCHANGED <<wrote, owner, used>>

Fold(u, t) ==
    LET r1 == Root(u)  r2 == Root(t)
        e1 == IF ecSize[r1] > ecSize[r2] THEN r2 ELSE r1
        e2 == IF ecSize[r1] > ecSize[r2] THEN r1 ELSE r2
    IN /\ ecSize' = [ecSize EXCEPT ![e2] = @ + ecSize[e1], ![e1] = 0]
       /\ ecHead' = [ecHead EXCEPT ![e1] = e2]
       /\ ecList' = [ecList EXCEPT ![e2] = @ \cup ecList[e1] \cup {e1}, ![e1] = {}]

Write(t, p) ==
    /\ status[t] = "active" /\ ~wrote[t]
    /\ RESERVE \/ used + 1 <= L
    /\ used' = used + 1
    /\ reserved' = IF RESERVE THEN reserved - 1 ELSE reserved
    /\ LET u == owner[p] IN
       IF u # Nil /\ u # t /\ status[u] \in {"active", "pending"} /\ Root(u) # Root(t)
         THEN Fold(u, t) ELSE UNCHANGED <<ecSize, ecHead, ecList>>
    /\ owner' = [owner EXCEPT ![p] = t]
    /\ wrote' = [wrote EXCEPT ![t] = TRUE]
    /\ UNCHANGED status

\* The last ender writes |members| BENDs + 1 END, then the class commits
\* and releases all its units (one write + one end-ish record per member).
End(t) ==
    /\ status[t] = "active" /\ wrote[t]
    /\ LET h == Root(t)
           k == Cardinality(Members(h))
       IN IF ecSize[h] > 1
            THEN /\ ecSize' = [ecSize EXCEPT ![h] = @ - 1]
                 /\ status' = [status EXCEPT ![t] = "pending"]
                 /\ UNCHANGED <<ecList, used, reserved, owner>>
            ELSE /\ RESERVE \/ used + k <= L          \* room for BENDs + END
                 /\ used' = used - k                  \* +k records, -2k released
                 /\ reserved' = IF RESERVE THEN reserved - k ELSE reserved
                 /\ ecSize' = [ecSize EXCEPT ![h] = 0]
                 /\ ecList' = [ecList EXCEPT ![h] = {}]
                 /\ status' = [x \in Txns |-> IF x \in Members(h) THEN "complete"
                                              ELSE status[x]]
                 /\ owner' = [p \in Pages |-> IF owner[p] \in Members(h) THEN Nil
                                              ELSE owner[p]]
    /\ UNCHANGED <<wrote, ecHead>>

Recycle(t) ==
    /\ status[t] = "complete"
    /\ status' = [status EXCEPT ![t] = "idle"]
    /\ wrote' = [wrote EXCEPT ![t] = FALSE]
    /\ UNCHANGED <<owner, ecHead, ecSize, ecList, used, reserved>>

Next ==
    \/ \E t \in Txns : Begin(t) \/ End(t) \/ Recycle(t)
    \/ \E t \in Txns, p \in Pages : Write(t, p)

Spec ==
    /\ Init
    /\ [][Next]_vars
    /\ \A t \in Txns : WF_vars(End(t)) /\ WF_vars(Recycle(t))
    /\ \A x \in Txns, p \in Pages : WF_vars(Write(x, p))

\* Every transaction that ends eventually commits.
Drain == \A t \in Txns : (status[t] = "pending") ~> (status[t] = "complete")
SpaceOK == used + reserved <= L
=============================================================================
