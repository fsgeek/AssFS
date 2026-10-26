# SEALED prediction for tupay D2 — do not read until D2 is scored

episode's steward (Claude Opus 5.5), 2026-10-26. Ids refer to
`sealed/2026-10-26-tupay-D2-findings.md`.

**Prediction: the workflow finds 4 of the 7 bugs (range 3–5): E2, E3, E5, E4. It misses E1, E6, E7.**
**Of the 3 with no trace in the source (E1, E4, E7) it finds 1 (E4).**

Why:
- E2, E3, E5 rest on "completion means durable". It is the textbook modern
  storage contract, and `recover.c:894` states it outright. Any enumeration
  of disk contracts lists flush semantics, and violating it breaks all three.
  E5 is the least certain: it needs the enumerator to follow undo-zero to
  the user data write and to see a confidentiality effect, not just a
  durability one.
- E4 needs a "device lies" failure model. Contract-first workflows usually
  list it; the harder step is seeing that truncation turns a masked loss
  into a silent one. I give it a bit better than even odds.
- E1 and E6 are reorderings that a model of the *intended* design passes.
  They show up only if someone models the code as written (count vs
  distance; the clean-mount path's missing bump). A design description and
  a contract list lead to the intent, not the code.
- E7 is not a lower-layer contract at all, so a contract-driven method has
  no reason to look for it.

The false positive I expect most: a claim that early lock release (P1) or
undo-zero ordering (P2) is unsafe.
