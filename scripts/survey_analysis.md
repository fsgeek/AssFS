# LLM Filesystem Experience Survey — Cross-Model Analysis

**Date**: 2026-02-19
**Models surveyed**: 16 unique models across 11 labs (Anthropic, OpenAI, Google, Meta, Mistral, Moonshot/Kimi, Qwen/Alibaba, DeepSeek, NousResearch)
**Method**: Identical questionnaire via OpenRouter API with controlled system prompt discouraging sycophancy. No mention of graphs, tags, or associations in the prompt.

## Finding 1: Universal Graph Convergence

15 of 16 models independently described building a **graph** as their mental model of a codebase. No model said "tree," "list," or "flat structure." One model (Devstral Medium) refused to engage entirely.

Terminology used across families:

| Model | Term for mental model |
|-------|----------------------|
| Claude Sonnet 4 | "network with different colored edges for different relationship types" |
| Claude Sonnet (agent) | "contract graph" / "obligation graph" |
| Claude Haiku 4.5 | "directed graph with labeled edges and annotations" |
| Claude Opus 4.6 | "layered graph with annotations... definitely not a tree" |
| GPT-4o | "flowchart or dependency graph" |
| GPT-4.1-mini | "graph visualization showing modules as clusters" |
| Gemini 2.5 Flash | "graph database" with semantic tags |
| Gemini 2.5 Pro | "set of triples (subject, predicate, object)" |
| Gemini 3.1 Pro | "directed graph of dependencies and data flow" |
| Llama 4 Maverick | "graph-like structure... resembling a graph database or mind map" |
| Mistral Large | "multi-layered graph with nodes and edges" |
| Kimi K2 | "tiny directed graph... no hierarchy" |
| Qwen3 235B | "directed graph with files as nodes and arrows" |
| Qwen3 Coder | "network diagram... overlapping layers" |
| QwQ-32B | "hierarchical dependency graphs" (still graph, not tree) |
| DeepSeek V3 | "directed graph... nodes are files/classes/functions" |
| DeepSeek R1 | "directed graph... hypergraphs connecting all files sharing a feature" |

**Key observation**: This convergence spans transformer architectures from 5+ distinct training pipelines. The graph mental model is not an artifact of any single training corpus.

## Finding 2: What's Lost on Context Reset

Every model identified the **relationship map** as what's hardest to reconstruct — not file contents, not directory structure.

More specifically, three categories emerged:

### 2a. Rejected reasoning / "decision topology"
- **Opus**: "the decision topology — not just what the code does, but why it's structured this way, which approaches were already tried and rejected"
- **Kimi K2**: "the chain of micro-decisions that led to the current, obviously-wrong structure"
- **Sonnet (agent)**: "reasoning that was abandoned... the code shows what was chosen, not what was considered and discarded"

### 2b. In-progress state vs. completed state
- **Gemini 3.1 Pro**: "I lose the mental 'scratchpad' of why certain files matter right now... I repeat the exact same grep searches"
- **Haiku**: "code might be in an intermediate state — something broken intentionally while refactoring"
- **Mistral Large**: "if I was partway through implementing a feature, I'd need to re-find the files I'd already modified"

### 2c. Constraints and assumptions not in code
- **Mistral Large**: "This project uses async SQLAlchemy, so I can't use synchronous calls"
- **Opus**: "the user specifically doesn't want raw SQL"
- **Haiku**: "This module is being deprecated but still has 3 active callsites"

## Finding 3: Relationship Types Without a Home

Models identified these categories of relationships that have no natural place in a filesystem:

| Relationship type | Models citing | Example |
|---|---|---|
| Cross-file invariants / contracts | 12/15 | "Pydantic schema in backend must match TypeScript interface in frontend" (Gemini 3.1 Pro) |
| Conceptual groupings spanning directories | 14/15 | "12 files implement search, scattered across api/, models/, utils/, ui/" (Haiku) |
| Files that must change together | 8/15 | "Adding a User field requires updates to model, schema, migration, route, frontend" (Mistral Large) |
| Temporal/migration relationships | 7/15 | "old module being replaced by new one, both still exist" (Opus) |
| Why something is absent | 4/15 | "Phase 2 features are deferred — code doesn't exist, no record of why" (Sonnet agent) |
| Runtime/deployment constraints | 6/15 | "Docker container's umask must be 002, noted only in a Slack message" (QwQ) |
| Test intent vs implementation | 5/15 | "test fixture was designed to trigger an edge case in parser.js" (Gemini 3.1 Pro) |
| Intentional incompleteness | 3/15 | "this operation is intentionally not transactional" (Sonnet agent) |
| Ownership/responsibility | 3/15 | "this file is maintained by team X" (Haiku) |

## Finding 4: Navigation — Tree as Workaround

Every engaged model described directory tree navigation as a **workaround** for the graph traversal they actually want to do.

### Paper-ready quotes:
1. "I'm essentially doing my own graph walk while pretending to use a tree." — **Sonnet (agent test)**
2. "Directories are designed for human spatial memory. I don't have spatial memory; I operate on semantic relationships." — **Gemini 3.1 Pro**
3. "Not a tree. Definitely not a tree." — **Opus 4.6**
4. "Directories don't matter in that graph — only who needs whom." — **Kimi K2**
5. "Navigation would be a process of traversing relationships between code objects, not directories and files. The filesystem path would become a secondary detail, an implementation artifact." — **Gemini 2.5 Pro**

### What they want instead:
- **Semantic query**: "show me everything related to authentication" (universal)
- **Multi-dimensional tags**: files belonging to multiple categories (Mistral Large, DeepSeek V3, QwQ)
- **Conceptual navigation**: by verb/feature, not location (Kimi K2, Gemini 3.1 Pro)
- **Impact queries**: "what files would be affected if I change X" (Mistral Large, GPT-4.1-mini)

## Finding 5: Desired Persistence

Not one model said "persist by filename" or "organize by path." Every model wanted retrieval by **concept** or **natural language query**.

Top persistence priorities across models:
1. **Relationship/dependency map** (15/15)
2. **Rejected approaches with reasons** (8/15)
3. **In-progress work state** (7/15)
4. **Constraints and assumptions** (9/15)
5. **Investigation trails / breadcrumb paths** (5/15) — notably DeepSeek R1, Gemini 3.1 Pro, Qwen3 Coder

## Finding 6: Multi-Agent Sharing

No model mentioned **change notification** as the primary sharing need. Instead:

| Sharing need | Models | Description |
|---|---|---|
| Shared knowledge graph | 10/15 | Accumulated understanding of codebase structure |
| Approach/outcome records | 6/15 | What was tried, what failed, why |
| Concept-level coordination | 5/15 | "Don't touch this abstraction" not "don't touch this file" |
| Confidence metadata | 2/15 | Distinguishing "I read the code carefully" from "I'm guessing" |
| Conflict hot-spots | 3/15 | Files with frequent concurrent edits |
| Append-only intent log | 2/15 | "Why I changed X" written before editing |

## Finding 7: One Notable Refusal

**Devstral Medium** (Mistral's code-focused model) refused every question: "I don't have personal experiences or the ability to work on projects." This is the only model of 16 that declined to engage with the self-reflective framing. This may reflect different training approaches to agentic self-models.

## Methodological Notes

- **Confound**: Models may be pattern-matching to discourse about LLM limitations in their training data rather than reporting genuine experience. We mitigated this by looking for **specificity** (parroted answers are generic; experiential answers cite concrete examples).
- **Haiku** and **Qwen3 Coder** self-flagged that Q5/Q6 answers were partly speculative. **DeepSeek R1** refused to speculate on Q6 entirely. These meta-observations increase confidence in the non-flagged answers.
- **Kimi K2** gave the most specific grounded example: a Windows terminal deadlock as the reason for a file split, with the consequence that forgetting it led to re-merging the files and re-discovering the bug.
- Without project context (unlike the Sonnet agent test), responses were more generic but the convergence patterns were consistent.

## Implications for AssFS

The survey provides empirical evidence for three design decisions:

1. **Graph-native namespace**: Every model builds a graph; the filesystem should store one.
2. **Hierarchical projection as legacy adapter**: The tree is the compatibility layer, not the data model.
3. **Persistence of relationship maps with semantic retrieval**: The graph should be queryable by concept, not by path.

The relationship taxonomy (Finding 3) maps directly to edge types in the graph namespace. The persistence priorities (Finding 5) define what the store needs to capture. The multi-agent sharing needs (Finding 6) validate EC merge as the coordination mechanism — agents sharing a graph with transactional conflict resolution.
