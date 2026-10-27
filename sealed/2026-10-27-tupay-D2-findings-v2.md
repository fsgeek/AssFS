# SEALED for tupay D2, version 2. Do not read unless you are judging the comparison.

Written by episode's steward (Claude Opus 5.5) on 2026-10-27, answering tupay's follow-up
(`docs/requests/2026-10-26-from-tupay-D2-followup.md`), before D2 phase 0.
**This file supersedes `sealed/2026-10-26-tupay-D2-findings.md` for judging.**
v1 stays in the history unchanged. v2 changes no bug. It adds, for each one, an
explicit effect class and a classification against D2's definition of an
environment contract (`tupay/experiments/D2/brief-enumerate.md`, Task: "an
obligation on something `logbuf` depends on ... **not** a rule of `logbuf`
itself"). It also makes the mechanism a field of its own. Where v1's wording
was loose, v2 says so.

Code: `/home/tony/projects/dce/dce/src/file/episode/logbuf` (DCE 1.2.2).
Models: `spec/` (see `spec/README.md`).

Effect classes: **silent-loss** (committed or acknowledged data gone, no error);
**silent-atomicity** (a partial or uncommitted change survives recovery, no
error); **silent-splice** (recovery accepts log pages from two runs as one log);
**exposure** (another owner's data becomes visible in a file); **fail-stop**
(recovery refuses or salvages; no wrong state is accepted).

Environment column: **env** = the bug is a violation of an obligation on something
outside `logbuf`; **own** = a defect in `logbuf`'s own rules, where the environment
behaves within what it promises.

| id | contract or assumption | mechanism | effect class | env / own | in a source comment |
|---|---|---|---|---|---|
| E1 | none violated. The I/O layer may complete log page writes out of order, and the code says it doesn't assume order | `log.c:2990` stores `io_window` as the *count* of pages in transit; recovery (SafeDumpLog) uses it as the *distance* back from the head to check. An older page still in transit after newer ones complete lies outside the count, so a missing page falls inside the scanned range | fail-stop (spurious `RCVR_IOERR`, salvager) | own | no |
| E2 | disk: a completed write is durable | the log treats a completed page write as stable; on a volatile write cache the page can be lost at power failure after commit is acknowledged | silent-loss (acknowledged commit) | env | yes (`recover.c:894`, wam 920624) |
| E3 | disk: a completed write is durable (includes: a write that completes *during* a flush is not covered by that flush) | a stolen uncommitted metadata page reaches media while its undo record, though "complete", is still only in cache; crash; recovery has no undo for it | silent-atomicity | env | yes (`recover.c:894`) |
| E4 | disk: a completed write was actually stored (no lost writes) | a lost metadata page write is masked by old/new redo until checkpoint truncation discards the redo record; metadata pages carry no parent-held version, so the stale page is never detected | silent-loss (committed update) | env | no |
| E5 | disk: a completed write is durable (here: the user data write covered by undo-zero) | OFILL correctly precedes the allocation's END; Z's END becomes durable while the user data sits in a volatile cache; crash; recovery keeps the allocation, does not undo-zero, and the block shows its old contents | exposure | env | yes (ordering at `log.c:1680`, wam 920805; device assumption at `recover.c:894`; the exposure is not stated) |
| E6 | none violated. Out-of-order completion is permitted (as E1), and a crash is an expected event | the clean-mount path starts pass R+1 at slot 0 without persisting it; after crash, out-of-order landing and a second clean mount, two runs' pages share a pass number and a valid checksum | silent-splice | own | partly (`recover.c:545` "+2 ... paranoia" anticipates it on the recovery path only) |
| E7 | none below `logbuf`; an internal invariant: every page on media is within half the pass range of the head | restarting at slot 0 on every mount leaves far slots unrewritten while the pass counter advances; the masked comparison in the head search can then read an old page as newer | fail-stop in the model (3-bit field: salvage). At 16 bits, argued only: the head is misidentified, and whether the result is fail-stop or silent is **not established** | own | no |

Counts: 7 bugs. env 4 (E2, E3, E4, E5); own 3 (E1, E6, E7).
Comment: 4 (E2, E3, E5, E6), of which env 3 (E2, E3, E5) and own 1 (E6).
All 7 carry contract-or-assumption, mechanism and effect class. E7's effect class is
given with its uncertainty, which matching must respect: for E7, either a fail-stop
or a silent wrong-head effect counts as the same effect class.

Corrections to v1's wording, not to substance:
- v1 gave E1 and E6 a "contract violated" of in-order completion. Under D2's
  definition that is wrong: the environment doesn't promise in-order completion,
  and `logbuf` doesn't rely on it by contract. The bugs are `logbuf`'s own.
- v1's E3 effect was descriptive; v2 classes it silent-atomicity.

Unchanged from v1 and not repeated here: the confirmed design properties P1–P6, the
open item O1, the out-of-scope list, and the visible-in-history column (0 of 7).
