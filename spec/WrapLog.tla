------------------------------ MODULE WrapLog ------------------------------
(***************************************************************************)
(* Episode's circular log across mounts (logbuf/log.c elbl_Init, recover.c *)
(* SafeDumpLog, elbr_Recover step 5, elbr_GetCleanLogInfo).                 *)
(*                                                                          *)
(* Device: write-through (the 1992 contract), out-of-order completion.     *)
(* Slot 0 holds the restart block when the log is clean.                    *)
(*                                                                          *)
(* Mount:                                                                   *)
(*   - if slot 0's first record is RESTART: clean. passno =                 *)
(*     IncPassno(restart pass); writing starts again at slot 0.             *)
(*   - else recover: binary search for the head on pass numbers             *)
(*     ((low - cur) masked to the top pass bit, as wam 920717), window      *)
(*     check from the head's io_window (distance form), scan tail..head;    *)
(*     then write a checkpoint block after the head and a restart block     *)
(*     with pass + 2 at slot 0, and mount clean.                            *)
(*                                                                          *)
(* Every page carries hidden ghost fields inc (mount incarnation) and seq   *)
(* (position within that incarnation) so we can say what recovery SHOULD   *)
(* have accepted.                                                           *)
(*                                                                          *)
(* FIX: a clean mount synchronously rewrites the restart block with the    *)
(* new pass number before writing any log page.                            *)
(***************************************************************************)
EXTENDS Naturals, FiniteSets

CONSTANTS N, W, K, MaxInc, FIX

Mod     == 2^W
Slots   == 0..(N - 1)
IncP(p) == IF (p + 1) % Mod = 0 THEN (p + 2) % Mod ELSE (p + 1) % Mod
DecP(p) == IF (p + Mod - 1) % Mod = 0 THEN (p + Mod - 2) % Mod ELSE (p + Mod - 1) % Mod
HighBit(d) == (d \div 2^(W - 1)) % 2 = 1
Dist(a, b) == (a + Mod - b) % Mod              \* low - current, unsigned, W bits

Page(k, pass, slot, iow, tail, inc, seq) ==
    [kind |-> k, pass |-> pass, pno |-> slot, iow |-> iow, tail |-> tail,
     inc |-> inc, seq |-> seq]
InitPage(s) == Page("init", 0, s, 0, s, 0, 0)

VARIABLES media, alive, inc, pass, cur, seq, queue, bad, salvaged

vars == <<media, alive, inc, pass, cur, seq, queue, bad, salvaged>>

Init ==
    /\ media = [s \in Slots |-> IF s = 0 THEN Page("rstrt", Mod - 4, 0, 0, 0, 0, 0)
                                ELSE InitPage(s)]
    /\ alive = FALSE
    /\ inc = 0 /\ pass = 0 /\ cur = 0 /\ seq = 0
    /\ queue = {}
    /\ bad = FALSE /\ salvaged = FALSE

(* ---------------- recovery, as SafeDumpLog ---------------- *)
CksumOK(pg) == pg.kind \in {"log", "ckpt"}
ValidActive(h, pg) ==
    IF pg.pno <= h.pno THEN pg.pass = h.pass ELSE pg.pass = DecP(h.pass)

RECURSIVE BS(_, _)
BS(lo, hi) ==
    LET c == (lo + hi) \div 2 IN
    IF c = lo \/ c = hi THEN <<lo, hi>>
    ELSE LET d == IF media[c].pass = 0 THEN 1 ELSE Dist(media[lo].pass, media[c].pass)
         IN IF d = 0 \/ HighBit(d) THEN BS(c, hi) ELSE BS(lo, c)

LoHi    == BS(0, N - 1)
Current == IF Dist(media[LoHi[1]].pass, media[LoHi[2]].pass) = 0
             THEN LoHi[2] ELSE LoHi[1]

\* Window check, oldest first: first invalid page pulls head to its predecessor.
WinBad == {i \in 0..media[Current].iow :
              LET pg == media[(Current - i + N) % N]
              IN ~CksumOK(pg) \/ ~ValidActive(media[Current], pg)}
RHead == IF WinBad = {} THEN Current
         ELSE LET i == CHOOSE j \in WinBad : \A k \in WinBad : j >= k
              IN (((Current - i + N) % N) - 1 + N) % N

HP     == media[RHead]
Tail   == HP.tail
Span   == IF Tail <= RHead THEN Tail..RHead ELSE (Tail..(N - 1)) \cup (0..RHead)
NoLegal == media[0].pass = 0
Salvage == NoLegal \/ \E s \in Span : ~CksumOK(media[s]) \/ ~ValidActive(HP, media[s])

Accepted == {media[s] : s \in {x \in Span : media[x].kind = "log"}}

\* Ground truth: the crashed incarnation's durable prefix (slots 0..k-1,
\* seq 1..k), or nothing if its slot 0 never landed.
Prefix(i) == {media[s] : s \in {x \in Slots : media[x].kind = "log" /\ media[x].inc = i
                                         /\ \A y \in 0..x : media[y].kind = "log"
                                                           /\ media[y].inc = i}}
RightAnswer == Accepted = Prefix(inc)

(* ---------------- mount ---------------- *)
StartClean(r) ==   \* r: restart pass on slot 0
    LET np == IncP(r) IN
    /\ pass' = np
    /\ cur' = 0 /\ seq' = 0 /\ queue' = {}
    /\ inc' = inc + 1
    /\ alive' = TRUE

MountClean ==
    /\ ~alive /\ ~salvaged /\ inc < MaxInc
    /\ media[0].kind = "rstrt"
    /\ StartClean(media[0].pass)
    /\ media' = IF FIX THEN [media EXCEPT ![0] = Page("rstrt", IncP(media[0].pass), 0, 0, 0, 0, 0)]
                ELSE media
    /\ UNCHANGED <<bad, salvaged>>

MountRecover ==
    /\ ~alive /\ ~salvaged /\ inc < MaxInc
    /\ media[0].kind # "rstrt"
    /\ IF Salvage
         THEN /\ salvaged' = TRUE
              /\ UNCHANGED <<media, alive, inc, pass, cur, seq, queue, bad>>
         ELSE LET h  == RHead
                  cs == (h + 1) % N
                  cp == IF h + 1 >= N THEN IncP(HP.pass) ELSE HP.pass
                  rp == (HP.pass + 2) % Mod                 \* literal "+ 2"
                  m1 == [media EXCEPT ![cs] = Page("ckpt", cp, cs, 0, cs, 0, 0)]
                  m2 == [m1 EXCEPT ![0] = Page("rstrt", rp, 0, 0, 0, 0, 0)]
              IN /\ bad' = (bad \/ ~RightAnswer)
                 /\ media' = IF FIX THEN [m2 EXCEPT ![0] = Page("rstrt", IncP(rp), 0, 0, 0, 0, 0)]
                             ELSE m2
                 /\ StartClean(rp)
                 /\ UNCHANGED salvaged

(* ---------------- running ---------------- *)
InFlight == {w.slot : w \in queue}
Submit ==
    /\ alive /\ seq < K /\ cur \notin InFlight
    /\ LET iow == IF InFlight = {} THEN 0
                  ELSE seq - (CHOOSE q \in {w.seq : w \in queue} :
                                  \A r \in {w.seq : w \in queue} : q <= r) + 1
       IN queue' = queue \cup {[slot |-> cur, seq |-> seq + 1,
                                pg |-> Page("log", pass, cur, iow, 0, inc, seq + 1)]}
    /\ seq' = seq + 1
    /\ cur' = (cur + 1) % N
    /\ pass' = IF cur + 1 >= N THEN IncP(pass) ELSE pass
    /\ UNCHANGED <<media, alive, inc, bad, salvaged>>

Complete(w) ==
    /\ alive /\ w \in queue
    /\ queue' = queue \ {w}
    /\ media' = [media EXCEPT ![w.slot] = w.pg]
    /\ UNCHANGED <<alive, inc, pass, cur, seq, bad, salvaged>>

Crash ==
    /\ alive
    /\ alive' = FALSE /\ queue' = {}
    /\ UNCHANGED <<media, inc, pass, cur, seq, bad, salvaged>>

\* Clean shutdown: everything durable, restart record with current pass.
Shutdown ==
    /\ alive /\ queue = {}
    /\ media' = [media EXCEPT ![0] = Page("rstrt", pass, 0, 0, 0, 0, 0)]
    /\ alive' = FALSE
    /\ UNCHANGED <<inc, pass, cur, seq, queue, bad, salvaged>>

Next == MountClean \/ MountRecover \/ Submit \/ Crash \/ Shutdown
        \/ \E w \in queue : Complete(w)

Spec == Init /\ [][Next]_vars

NoSilentError == ~bad
NoSalvage     == ~salvaged
=============================================================================
