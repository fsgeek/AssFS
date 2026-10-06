------------------------------ MODULE WrapResume ------------------------------
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

CONSTANTS N, W, K, MaxInc, FIX, CHAIN, SCRUB, IOMAX
\* IOMAX: a page may be submitted at most IOMAX-1 pages beyond the oldest
\* page still in flight (bounds io_window as a distance).
\* SCRUB: at mount, durably overwrite the IOMAX slots after the recovered
\* head (where a crashed run's stale pages can be) before resuming.
\* CHAIN: each page records the identity of the page written before it;
\* recovery walks forward from the tail and stops at the first mismatch.
\* Variant: the log RESUMES after the recovered head instead of restarting
\* at slot 0. Clean shutdown writes its restart record as the next page in
\* sequence; every mount finds the head; the log is clean if the head page
\* is a restart or checkpoint record. FIX persists a pass bump first.

Mod     == 2^W
Slots   == 0..(N - 1)
IncP(p) == IF (p + 1) % Mod = 0 THEN (p + 2) % Mod ELSE (p + 1) % Mod
DecP(p) == IF (p + Mod - 1) % Mod = 0 THEN (p + Mod - 2) % Mod ELSE (p + Mod - 1) % Mod
HighBit(d) == (d \div 2^(W - 1)) % 2 = 1
Dist(a, b) == (a + Mod - b) % Mod              \* low - current, unsigned, W bits

Page(k, pass, slot, iow, tail, inc, seq) ==
    [kind |-> k, pass |-> pass, pno |-> slot, iow |-> iow, tail |-> tail,
     inc |-> inc, seq |-> seq, prev |-> <<>>]
Id(pg) == <<pg.kind, pg.inc, pg.seq, pg.pass, pg.pno>>
InitPage(s) == Page("init", 0, s, 0, s, 0, 0)

VARIABLES media, alive, inc, pass, cur, seq, queue, bad, salvaged, start, shut, lastId

vars == <<media, alive, inc, pass, cur, seq, queue, bad, salvaged, start, shut, lastId>>

Init ==
    /\ media = [s \in Slots |-> IF s = 0
                                  THEN [Page("rstrt", Mod - 4, 0, 0, 0, 0, 0)
                                          EXCEPT !.prev = Id(InitPage(N - 1))]
                                ELSE InitPage(s)]
    /\ alive = FALSE
    /\ inc = 0 /\ pass = 0 /\ cur = 0 /\ seq = 0
    /\ queue = {}
    /\ bad = FALSE /\ salvaged = FALSE
    /\ start = 1
    /\ shut = FALSE
    /\ lastId = <<>>

(* ---------------- recovery, as SafeDumpLog ---------------- *)
CksumOK(pg) == pg.kind \in {"log", "ckpt", "rstrt", "fill"}
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
RHead0 == IF WinBad = {} THEN Current
         ELSE LET i == CHOOSE j \in WinBad : \A k \in WinBad : j >= k
              IN (((Current - i + N) % N) - 1 + N) % N

Tail0  == media[RHead0].tail
Len0   == ((RHead0 - Tail0 + N) % N) + 1
SlotAt(j) == (Tail0 + j) % N
Linked(j) == media[SlotAt(j)].prev = Id(media[(SlotAt(j) + N - 1) % N])
\* The tail page is the anchor; links are checked from tail+1 to head.
ChainHead == LET good == {j \in 0..(Len0 - 1) : \A i \in 1..j : Linked(i)}
             IN SlotAt(CHOOSE j \in good : \A k \in good : j >= k)
RHead  == IF CHAIN THEN ChainHead ELSE RHead0
HP     == media[RHead]
Tail   == HP.tail
Span   == IF Tail <= RHead THEN Tail..RHead ELSE (Tail..(N - 1)) \cup (0..RHead)
NoLegal == media[0].pass = 0
Salvage == NoLegal \/ \E s \in Span : ~CksumOK(media[s]) \/ ~ValidActive(HP, media[s])

Accepted == {media[s] : s \in {x \in Span : media[x].kind = "log"}}

\* Ground truth: the crashed incarnation's durable prefix (slots 0..k-1,
\* seq 1..k), or nothing if its slot 0 never landed.
At(j) == media[(start + j - 1) % N]
DurLen == CHOOSE k \in 0..K : (\A j \in 1..k : At(j).kind = "log" /\ At(j).inc = inc
                                              /\ At(j).seq = j)
                            /\ (k = K \/ ~(At(k + 1).kind = "log" /\ At(k + 1).inc = inc
                                            /\ At(k + 1).seq = k + 1))
Prefix(i) == {At(j) : j \in 1..DurLen}
RightAnswer == Accepted = Prefix(inc)

(* ---------------- mount ---------------- *)
\* Resume at slot c with pass p (p already the pass for slot c).
Resume(c, p, m) ==
    LET np == IF FIX THEN IncP(p) ELSE p IN   \* FIX: bump and persist
    /\ lastId' = Id(m[(c + N - 1) % N])
    /\ pass' = np /\ cur' = c /\ seq' = 0 /\ queue' = {}
    /\ start' = c
    /\ inc' = inc + 1 /\ alive' = TRUE /\ shut' = FALSE

PassAt(prev, c) == IF c = 0 THEN IncP(prev) ELSE prev   \* wrap bumps pass

Mount ==
    /\ ~alive /\ ~salvaged /\ inc < MaxInc
    /\ IF Salvage
         THEN /\ salvaged' = TRUE
              /\ UNCHANGED <<media, alive, inc, pass, cur, seq, queue, bad, start, shut, lastId>>
         ELSE LET h == RHead
                  clean == HP.kind \in {"rstrt", "ckpt"}
                  cs == (h + 1) % N
                  cp == PassAt(HP.pass, cs)
                  ck == [Page("ckpt", cp, cs, 0, cs, inc, 0) EXCEPT !.prev = Id(HP)]
                  rs == IF clean THEN cs ELSE (h + 2) % N
                  rp == IF clean THEN cp ELSE PassAt(cp, rs)
                  ahead == {(h + i) % N : i \in 1..IOMAX}
                  \* filler carries the pass BEFORE the one the next run uses there
                  FillPass(x) == DecP(IF x < rs THEN IncP(rp) ELSE rp)
                  m0 == IF SCRUB THEN [x \in Slots |-> IF x \in ahead
                                         THEN Page("fill", FillPass(x), x, 0, x, inc, 0)
                                         ELSE media[x]]
                        ELSE media
                  m1 == IF clean THEN m0 ELSE [m0 EXCEPT ![cs] = ck]
                  \* FIX: persist the bumped pass as a checkpoint page at rs-1
                  m2 == IF FIX THEN [m1 EXCEPT ![(rs + N - 1) % N] =
                                       Page("ckpt", IncP(rp), (rs + N - 1) % N, 0,
                                            (rs + N - 1) % N, 0, 0)]
                        ELSE m1
              IN /\ bad' = (bad \/ (clean /\ ~shut /\ Prefix(inc) # {})
                                \/ (~clean /\ ~RightAnswer))
                 /\ media' = m2
                 /\ Resume(rs, rp, m2)
                 /\ UNCHANGED salvaged

(* ---------------- running ---------------- *)
InFlight == {w.slot : w \in queue}
Submit ==
    /\ alive /\ seq < K /\ cur \notin InFlight
    \* Bound the DISTANCE to the oldest unfinished page, not the count:
    \* the new page (seq+1) may be at most IOMAX past the last durable one.
    /\ \A w \in queue : (seq + 1) - w.seq < IOMAX
    /\ LET iow == IF InFlight = {} THEN 0
                  ELSE seq - (CHOOSE q \in {w.seq : w \in queue} :
                                  \A r \in {w.seq : w \in queue} : q <= r) + 1
           pg  == [Page("log", pass, cur, iow, (start + N - 1) % N, inc, seq + 1) EXCEPT !.prev = lastId]
       IN /\ queue' = queue \cup {[slot |-> cur, seq |-> seq + 1, pg |-> pg]}
          /\ lastId' = Id(pg)
    /\ seq' = seq + 1
    /\ cur' = (cur + 1) % N
    /\ pass' = IF cur + 1 >= N THEN IncP(pass) ELSE pass
    /\ UNCHANGED <<media, alive, inc, bad, salvaged, start, shut>>

Complete(w) ==
    /\ alive /\ w \in queue
    /\ queue' = queue \ {w}
    /\ media' = [media EXCEPT ![w.slot] = w.pg]
    /\ UNCHANGED <<alive, inc, pass, cur, seq, bad, salvaged, start, shut, lastId>>

Crash ==
    /\ alive
    /\ alive' = FALSE /\ queue' = {}
    /\ UNCHANGED <<media, inc, pass, cur, seq, bad, salvaged, start, shut, lastId>>

\* Clean shutdown: everything durable; restart record as the next page.
Shutdown ==
    /\ alive /\ queue = {}
    /\ media' = [media EXCEPT ![cur] =
                     [Page("rstrt", pass, cur, 0, cur, inc, 0) EXCEPT !.prev = lastId]]
    /\ alive' = FALSE /\ shut' = TRUE
    /\ UNCHANGED <<inc, pass, cur, seq, queue, bad, salvaged, start, lastId>>

Next == Mount \/ Submit \/ Crash \/ Shutdown
        \/ \E w \in queue : Complete(w)

Spec == Init /\ [][Next]_vars

NoSilentError == ~bad
NoSalvage     == ~salvaged
=============================================================================
