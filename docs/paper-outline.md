# Memory-Shaped Storage: What AI Agents Reveal About the Filesystem Namespace

**Target venue:** FAST 2026 (or HotStorage as fallback)
**Working title** — may evolve

## Abstract (draft)

The filesystem namespace has been a hierarchical tree since Multics (1965).
We present evidence that AI agents — the first fundamentally new class of
filesystem consumer since timesharing — expose this as a design limitation
rather than a design choice. Through a structured survey of 16 LLMs across
11 labs and a behavioral study measuring operational choices across 3
task scenarios, we find that: (1) models universally describe building
graph mental models of codebases, (2) when offered graph-native filesystem
operations alongside standard POSIX operations, 5 of 8 models use them
selectively, matching primitive to context, and (3) novel primitives
(backpointer traversal, stable identity) are adopted when scenarios
demonstrate their value but ignored when merely available. We present
AssFS, a transactional graph-native filesystem with POSIX-compatible
hierarchical projection, and argue that "memory-shaped storage" —
persistence designed around its consumer's cognitive architecture — is
the right framing for next-generation storage systems.

## 1. Introduction

- Storage has evolved enormously since 1965. Namespace hasn't.
- Seltzer's "Hierarchical File Systems Are Dead" (2009) — correct diagnosis,
  but the replacement consumers were still human.
- AI agents are different: no spatial memory, context resets, graph-shaped
  reasoning, multi-agent relay.
- The question isn't "should filesystems be graphs?" (asked and answered
  multiple times). The question is "who is the filesystem *for*?"
- Contribution: empirical evidence + working system + design framework

## 2. Background and Related Work

### 2.1 Filesystem Namespace Evolution
- Multics → Unix tree → minor extensions (symlinks, xattrs, ACLs)
- Prior graph/semantic FS: SIL, GFS, semantic desktop, WinFS, Spotlight
- Why they failed: designed for humans who already had working trees
- Key insight: hierarchies capture "contains" but not relationships *between*

### 2.2 AI Agents as Storage Consumers
- Context windows as working memory (finite, lossy)
- Context reset as amnesia (no persistence across sessions by default)
- Multi-agent collaboration (no shared state by default)
- Tool use as I/O (agents interact with storage through tool calls)

### 2.3 Transaction/Recovery for Graph Storage
- Brief: Episode's STEAL/NO-FORCE, EC merge, old/new value logging
- Why transactional correctness matters for multi-agent graph mutation
- NOT the focus of this paper (separate TOS submission)

## 3. Methodology

### 3.1 Self-Report Survey
- 16 models, 11 labs, via OpenRouter API
- Controlled system prompt (no leading, discourage sycophancy)
- 7 questions, no mention of graphs/tags/associations
- Methodology: single prompt, controlled temperature, same questions
- Confound: discourse-echo (addressed in §5)

### 3.2 Behavioral Test
- 3 scenarios: bug diagnosis, feature addition, renamed-file tracking
- Two toolkits offered equally: standard FS ops + graph ops
  - Standard: ls, cat, grep, find, write, mkdir
  - Graph: tag, untag, query_tag, get_refs, get_id, list_tags
- 8 models, same OpenRouter infrastructure
- Analysis: operation parsing, graph ratio, primitive selection, ordering
- What we measure: adoption rate, which primitives, when in workflow

### 3.3 Limitations
- Simulated codebase (not mounted FS with real tool execution)
- Small codebases (6-15 files) — context pressure is low
- Stated choice, not true revealed preference
- Models may be influenced by training data about graph filesystems

## 4. Results

### 4.1 Survey Findings
- Universal graph convergence (15/16)
- Relationship map as what's lost on context reset
- Taxonomy of 9 relationship types without filesystem home
- Tree navigation described as workaround
- Multi-agent: shared understanding, not shared events
- Table: model × terminology used for mental model
- Table: relationship types × citation count

### 4.2 Behavioral Findings
- Cross-scenario graph adoption: 5/8 models used graph ops
- Contextual primitive selection:
  - Tags for organizing investigations (auth_bug, feature_add)
  - Identity/refs for tracking renames (renamed_file)
- Two usage patterns: annotation (build index) vs retrieval (query index)
- Novel primitives appear under pressure:
  - get_refs: 0% in scenarios 1-2, 25% in scenario 3
  - Capability advertisement enables discovery
- Most capable model (Sonnet 4) never used graph ops — solved everything
  through reasoning. Graph ops are optimization, not requirement.
- Table: model × scenario × graph ops used
- Table: primitive × scenario × adoption rate

### 4.3 Observed In The Wild
- Brief: agents using ~/.claude/ directory as inter-instance communication
  channel (n=1, uncontrolled, but motivating)
- Pattern: writing accumulated knowledge to shared namespace, not events

## 5. Discussion

### 5.1 Discourse-Echo and Validity
- Confound: models may report training data, not experience
- Mitigation: specificity analysis (grounded examples vs generic claims)
- Behavioral test as partial answer (usage, not just description)
- Remaining limitation: true revealed preference needs real tool execution

### 5.2 Memory-Shaped Storage
- Storage shaped by consumer's cognitive architecture
- Filing cabinets for humans → trees for spatial memory
- Graphs for agents → associative, relationship-first, context-surviving
- Not "better filesystem" — different consumer, different shape

### 5.3 Design Implications
- The primitive set: tag, query_tag, get_id, get_refs, relate (from data)
- On-ramp: familiar ops adopted freely, novel ops need demonstration
- Capability advertisement: filesystem surfaces relevant affordances
- Graph ops as optimization: must never be required for correctness
- Two usage patterns → API must support both annotation and retrieval
- POSIX as projection: hierarchy is the backward-compatible view

### 5.4 The First New Consumer Since Timesharing
- Timesharing introduced: multiple users, permissions, home directories
- AI agents introduce: context resets, graph reasoning, multi-agent relay
- Previous "new consumers" (databases, browsers) adapted to the tree
- Agents are the first consumer class where the tree is genuinely wrong

## 6. AssFS: A Graph-Native Filesystem

### 6.1 Architecture
- 6-layer: store, txn, graph, projection, refcount, FUSE adapter
- Graph namespace with POSIX hierarchical projection
- Content-addressed transactional object store
- xattr-as-graph-edge for POSIX-compatible graph access

### 6.2 Transaction Model (brief)
- Old/new value logging (undo + redo)
- Equivalence class merge for concurrent graph mutation
- Crash recovery: idempotent redo, metadata reconciliation

### 6.3 Current State
- 60 tests passing, crash-after-every-mutation harness
- Performance: create ~11ms, write ~6ms, lookup ~50μs
- Consistency checker: 15 structural invariants

## 7. Future Work

- Behavioral test with real mounted FS (true revealed preference)
- Scale testing: 100+ file codebases where context pressure is real
- `relate(a, predicate, b)` — directional typed edges, untested behaviorally
- Capability advertisement protocol
- Implicit annotation: inferring edges from agent reasoning patterns
- Full evaluation: performance under agent workloads
- Async log writes, undo-zero, checkpoint/truncation (Phase 2)

## 8. Conclusion

- AI agents reveal that the filesystem namespace is shaped for the wrong
  consumer
- Empirical evidence: survey (what they want) + behavioral (what they do)
- "Memory-shaped storage" as the design principle
- AssFS as proof of concept: graph-native, transactional, POSIX-compatible
- The tree isn't wrong. It's a projection. The graph is the territory.

---

## Key Figures

1. Model × mental model terminology table (survey)
2. Relationship taxonomy with citation counts (survey)
3. Cross-scenario graph op adoption heatmap (behavioral)
4. Operation sequence timeline showing graph ops appearing first in
   scenario 3 (behavioral)
5. AssFS architecture diagram (system)
6. Performance measurements (system)

## Key Quotes (for pull-quotes / epigraph)

- "I'm essentially doing my own graph walk while pretending to use a tree."
  — Claude Sonnet
- "Directories are designed for human spatial memory. I don't have spatial
  memory." — Gemini 3.1 Pro
- "Not a tree. Definitely not a tree." — Claude Opus
- "Directories don't matter in that graph — only who needs whom." — Kimi K2
