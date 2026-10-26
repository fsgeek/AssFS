# SEALED for tupay D2 — do not read unless you are judging the comparison

Written by episode's steward (Claude Opus 5.5), 2026-10-26, for the request in
`docs/requests/2026-10-26-from-tupay-D2-questions.md`. tupay's steward and
its authors must not open this file before every D2 finding is committed.
The reply in `docs/requests/` gives only its path, commit and sha256.

Source of every item: `spec/README.md` (TLA+ models in `spec/`, branch
`spec/tla-models`, merged to `main` 2026-10-26). Code as in
`/home/tony/projects/dce/dce/src/file/episode/logbuf` (DCE 1.2.2).

"Date" is the date the finding was committed to `spec/README.md`
(all 2026-10-06; the modelling sessions ran from about 2026-10-02).

"Visible in DCE" uses three bins, stricter than tupay's two:
- **history** — a later fix, changelog entry or erratum in DCE 1.2.2. The
  RCS `$Log` blocks in logbuf hold only OSF merge housekeeping; Transarc's
  internal history is not in the release. **No finding is in this bin.**
- **comment** — the enabling assumption or the hazard is stated in a source
  comment, but the failure itself is not.
- **analysis-only** — nothing in the source points at it.

## Bugs in Episode's log and recovery (7)

| id | description | contract violated (layer: what it failed to provide) | found by | visible in DCE | date |
|---|---|---|---|---|---|
| E1 | `io_window` stores a *count* of in-transit log pages (`log.c:2990`, `inTransit.length`); recovery reads it as a *distance* back from the head (`recover.c` SafeDumpLog). If an older page is still in transit when newer ones complete, the hole falls inside the scanned range: `RCVR_IOERR`, salvager. Fail-stop. | I/O layer: in-order completion of log page writes (the code itself says "we don't assume writes are in order"; the count is only right if they are) | model (EpisodeLog.tla, ep1992 vs ep1992-dist) | analysis-only | 2026-10-06 |
| E2 | An acknowledged commit is silently lost: the log treats a completed page write as stable. | disk: completion implies durability (fails with a volatile write cache, without a flush issued after completion) | model (EpisodeLog.tla ep-nvme-dist; StealNoForce) | comment (`recover.c:894`, wam 920624: "we trust the underlying system to really guarantee that a page is on disk when it says it is") | 2026-10-06 |
| E3 | A stolen (pre-commit) metadata page reaches media before its undo record does; a crash leaves an uncommitted change with no undo. | disk: completion implies durability (same as E2; also: a log write completing *during* a flush is not covered by it) | model (StealNoForce.tla nosnap) | comment (same, `recover.c:894`) | 2026-10-06 |
| E4 | A lost write (the device reports success without storing it) is masked by old/new redo until checkpoint truncation discards the redo record; after that the committed update is silently gone. Metadata pages carry no parent-held version to detect it. | disk: a completed write was actually stored (lying / lost-write device) | model (LostWrite.tla lw-trunc) | analysis-only | 2026-10-06 |
| E5 | Undo-zero exposure: with OFILL ordered correctly, Z's END can be durable while the user's data write sits in a volatile cache; after a crash the file shows the block's previous owner's data. A confidentiality failure. | disk: completion implies durability, for the user data write | model (UndoZero.tla uz-nvme) | comment (ordering requirement stated at `log.c:1680`, wam 920805; device assumption at `recover.c:894`; the exposure is not stated) | 2026-10-06 |
| E6 | Pass-number reuse after a crash right after a clean mount: the clean-mount path starts pass R+1 at slot 0 without persisting it; with out-of-order page completion and two crashes, two runs' pages carry the same pass and recovery replays them as one log. Silent. | I/O layer: in-order completion of log page writes (plus a crash before the first page set lands) | model (WrapLog.tla wrap-asis) | comment, partial (`recover.c:545` "+2 ... paranoia" anticipates the hazard on the recovery path only; the clean-mount path has no bump) | 2026-10-06 |
| E7 | Masked pass-number comparison assumes every page on media is within half the pass range of the head; restarting at slot 0 on every mount leaves far slots unrewritten while the counter advances, so the head search can read an old page as newer. ~10^4 mounts at 16 bits. | none below Episode: an internal invariant of the on-disk log format and mount path | argued; model shows it only with a 3-bit field (wrap-fix-long) | analysis-only | 2026-10-06 |

Counts: 7 bugs. history 0; comment 4 (E2, E3, E5, E6); analysis-only 3 (E1, E4, E7).
Under tupay's two-way split ("visible in DCE history" vs "found only by analysis"), all 7 are analysis-only: no fix, changelog or erratum exists for any of them.

Caveat recorded in `spec/README.md`: E1 and E6 need out-of-order completion of log page writes; whether 1990s I/O paths ever produced that is not established.

## Not bugs — for the judge, to score false positives

Design properties the models confirmed (a D2 finding that claims one of these fails is "no match", and should be examined as a possible error on either side):
- P1 Early lock release (at END append, before it is durable) is safe, because recovery stops at the first log gap. (StealNoForce / EpisodeLog)
- P2 The undo-zero "logically ordered" requirement is load-bearing: OFILL must precede the allocating transaction's END. As coded, it does. (uz-misorder fails only when the order is broken.)
- P3 Equivalence-class merge as coded (tran.c) is atomic; at full contention a class commit is a checkpoint. (ECMerge.tla)
- P4 Up-front log reservation lets a hot class always drain; on-demand allocation would deadlock. The `force` admission bypass (log.c:1678) is unmodelled. (ECDrain.tla)
- P5 Physical old/new logging tolerates torn page writes with no doublewrite buffer. (TornPage.tla)
- P6 The `B_ASYNC` precondition above `efs_ZeroBytes` (vnops/efs_newalloc.c) holds in anode/strategy.c. (code reading)

## Open, not counted

- O1 `epia_MarkInTransit` (anode/strategy.c) states but does not assert that "higher-level vnode locking" prevents a block re-entering transit between a reader's wait and its read. Load-bearing for reads. A call-graph audit, not done. A D2 finding here is "unresolved", neither match nor no match.

## Out of scope

- Linux jbd2 findings 14–17 in `spec/README.md` (not Episode).
- `ec-double` in ECMerge.tla is a bug in AssFS's reimplementation, not in Episode.
- Findings 10 and 11 are about a proposed repaired mount design, not the code.
