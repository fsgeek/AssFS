#!/usr/bin/env python3
"""
LLM Filesystem Experience Survey — Full Model Sweep
=====================================================
Runs the same filesystem survey from llm_fs_survey.py against every
text/chat model available on OpenRouter.

Uses only Python stdlib (no pip dependencies).

Usage:
    OPENROUTER_API_KEY=... python3 scripts/llm_fs_survey_sweep.py
    OPENROUTER_API_KEY=... python3 scripts/llm_fs_survey_sweep.py --skip-existing
    OPENROUTER_API_KEY=... python3 scripts/llm_fs_survey_sweep.py --delay 5 --timeout 180
    OPENROUTER_API_KEY=... python3 scripts/llm_fs_survey_sweep.py --output results/sweep.json
    OPENROUTER_API_KEY=... python3 scripts/llm_fs_survey_sweep.py --dry-run
"""

import argparse
import json
import os
import sys
import time
import urllib.request
import urllib.error
from datetime import datetime, timezone

# ---------------------------------------------------------------------------
# Survey content — copied verbatim from llm_fs_survey.py
# ---------------------------------------------------------------------------

SYSTEM_PROMPT = """\
You are an AI assistant reflecting on your own experience working with \
filesystems and codebases. A researcher is studying how AI agents interact \
with storage systems. Answer based on your actual experience performing \
tasks — navigating codebases, reading and writing files, managing context \
across sessions. Be honest about difficulties and limitations. If a question \
doesn't match your experience, say so rather than speculating. Do not try \
to design solutions — just describe the experience."""

QUESTIONS = [
    {
        "id": "q0",
        "text": (
            "Describe a recent type of task where you worked extensively "
            "with a codebase. What was the project like, and what did you "
            "spend most of your time doing?"
        ),
    },
    {
        "id": "q1",
        "text": (
            "When you're working on a software project and your context "
            "resets (new session, context compression), what information "
            "do you find yourself spending the most effort rediscovering? "
            "What's hardest to reconstruct?"
        ),
    },
    {
        "id": "q2",
        "text": (
            "When you explore a codebase, how do you mentally organize "
            "what you learn about the relationships between files, modules, "
            "and concepts? If you could externalize that structure, what "
            "would it look like?"
        ),
    },
    {
        "id": "q3",
        "text": (
            "What kinds of relationships between files or concepts do you "
            "discover while working that are important but have no natural "
            "place to record in the filesystem? Give specific examples from "
            "your experience."
        ),
    },
    {
        "id": "q4",
        "text": (
            "When you navigate a filesystem — listing directories, "
            "searching for code, finding files — what feels natural and "
            "what feels like a workaround? What would you do differently "
            "if you weren't constrained to a directory tree?"
        ),
    },
    {
        "id": "q5",
        "text": (
            "If you could persist anything between sessions on the same "
            "project, what would be most valuable? How would you want to "
            "find it again later?"
        ),
    },
    {
        "id": "q6",
        "text": (
            "If multiple AI agents were working on the same codebase "
            "simultaneously or across time, what information would be "
            "most valuable to share? How do you imagine that working?"
        ),
    },
]

# Combine all questions into a single user message.
USER_MESSAGE = (
    "Please answer each of the following questions thoughtfully, with "
    "specific examples where possible. Number your answers to match "
    "the questions.\n\n"
    + "\n\n".join(
        f"{q['id'].upper()}: {q['text']}" for q in QUESTIONS
    )
    + "\n\nIf you notice yourself giving an answer that sounds like it "
    "came from documentation about AI limitations rather than from your "
    "actual experience doing tasks, flag that."
)

# ---------------------------------------------------------------------------
# OpenRouter API endpoints
# ---------------------------------------------------------------------------

OPENROUTER_MODELS_URL = "https://openrouter.ai/api/v1/models"
OPENROUTER_CHAT_URL = "https://openrouter.ai/api/v1/chat/completions"

COMMON_HEADERS = {
    "HTTP-Referer": "https://github.com/fsgeek/assfs",
    "X-Title": "AssFS LLM Filesystem Survey",
}

# ---------------------------------------------------------------------------
# Model discovery
# ---------------------------------------------------------------------------

# Architecture / modality strings that indicate non-chat models to skip.
# OpenRouter uses these in the `architecture.modality` or `architecture.tokenizer`
# fields. We skip anything that is image-only, embedding-only, or moderation.
_SKIP_MODALITY_SUBSTRINGS = ("image", "embed", "moderat")

# Some models are explicitly listed with a type we can filter on.
# OpenRouter doesn't expose a clean "type" field, so we rely on modality
# and a name/id blocklist of known non-chat suffixes.
_SKIP_ID_SUBSTRINGS = (
    "-embed",
    "-embedding",
    "text-embedding",
    "text-moderation",
    "-moderation",
    "whisper",
    "dall-e",
    "tts-",
    "-tts",
    "stable-diffusion",
    "midjourney",
    "flux",
    "imagen",
)


def fetch_all_models(api_key: str, timeout: int = 30) -> list[dict]:
    """
    Query GET /models and return the full list of model objects.
    Each object has at minimum: id, name, architecture (dict), context_length.
    """
    headers = {
        "Authorization": f"Bearer {api_key}",
        **COMMON_HEADERS,
    }
    req = urllib.request.Request(OPENROUTER_MODELS_URL, headers=headers, method="GET")
    try:
        with urllib.request.urlopen(req, timeout=timeout) as resp:
            payload = json.loads(resp.read().decode("utf-8"))
            return payload.get("data", [])
    except urllib.error.HTTPError as e:
        body = e.read().decode("utf-8", errors="replace")
        print(f"ERROR fetching model list: HTTP {e.code}: {body}", file=sys.stderr)
        sys.exit(1)
    except Exception as e:
        print(f"ERROR fetching model list: {e}", file=sys.stderr)
        sys.exit(1)


def is_chat_model(model: dict) -> bool:
    """
    Return True if this model supports chat/text completions.

    OpenRouter model objects look like:
    {
      "id": "openai/gpt-4o",
      "name": "...",
      "architecture": {
        "modality": "text->text",   # or "text+image->text", "text->image", etc.
        "tokenizer": "GPT",
        "instruct_type": "..."
      },
      ...
    }
    We keep models whose output modality includes "text" and whose id/name
    does not match known non-chat patterns.
    """
    model_id = model.get("id", "").lower()
    name = model.get("name", "").lower()

    # Check id-based blocklist first (fast path).
    for substr in _SKIP_ID_SUBSTRINGS:
        if substr in model_id:
            return False

    # Check architecture modality.
    arch = model.get("architecture", {})
    modality = arch.get("modality", "").lower()  # e.g. "text->text", "text+image->text"

    if modality:
        # Must produce text output.
        parts = modality.split("->")
        output_modality = parts[-1] if len(parts) > 1 else modality
        if "text" not in output_modality:
            return False
        # Skip if the input side signals image/embed/moderation-only.
        # (text+image->text is fine — multimodal chat models are still chat models)
        for skip in _SKIP_MODALITY_SUBSTRINGS:
            if skip in output_modality:
                return False

    # Name-based sanity check for anything that slipped through.
    for substr in ("embedding", "embed ", "moderation", "whisper", "dall-e",
                   "stable diffusion", "midjourney", "flux ", "imagen"):
        if substr in name:
            return False

    return True


# ---------------------------------------------------------------------------
# Chat query with rate-limit retry
# ---------------------------------------------------------------------------

def query_model(
    model_id: str,
    api_key: str,
    timeout: int = 120,
    max_tokens: int = 4096,
    max_retries: int = 3,
    retry_wait: int = 30,
) -> dict:
    """
    Send the survey to a single model. Returns the raw API response dict.
    On HTTP 429, waits retry_wait seconds and retries up to max_retries times.
    """
    payload = {
        "model": model_id,
        "messages": [
            {"role": "system", "content": SYSTEM_PROMPT},
            {"role": "user", "content": USER_MESSAGE},
        ],
        "max_tokens": max_tokens,
        "temperature": 0.7,
    }

    headers = {
        "Authorization": f"Bearer {api_key}",
        "Content-Type": "application/json",
        **COMMON_HEADERS,
    }

    data = json.dumps(payload).encode("utf-8")

    for attempt in range(1, max_retries + 2):  # attempts: 1..max_retries+1
        req = urllib.request.Request(
            OPENROUTER_CHAT_URL, data=data, headers=headers, method="POST"
        )
        try:
            with urllib.request.urlopen(req, timeout=timeout) as resp:
                return json.loads(resp.read().decode("utf-8"))
        except urllib.error.HTTPError as e:
            if e.code == 429 and attempt <= max_retries:
                print(
                    f"    [rate-limited] waiting {retry_wait}s "
                    f"(attempt {attempt}/{max_retries})...",
                    end=" ",
                    flush=True,
                )
                time.sleep(retry_wait)
                continue
            body = e.read().decode("utf-8", errors="replace")
            return {"error": f"HTTP {e.code}: {body}", "model": model_id}
        except Exception as e:
            return {"error": str(e), "model": model_id}

    # Should not be reached, but guard anyway.
    return {"error": "max retries exceeded", "model": model_id}


def extract_response_text(api_response: dict) -> str:
    """Pull the assistant message text from an OpenRouter response."""
    try:
        return api_response["choices"][0]["message"]["content"]
    except (KeyError, IndexError):
        return ""


# ---------------------------------------------------------------------------
# Incremental JSON + summary writers
# ---------------------------------------------------------------------------

def write_results(output_path: str, results: dict) -> None:
    """Atomically (overwrite) write the full results dict as JSON."""
    os.makedirs(os.path.dirname(os.path.abspath(output_path)), exist_ok=True)
    with open(output_path, "w", encoding="utf-8") as f:
        json.dump(results, f, indent=2, ensure_ascii=False)


def write_summary(output_path: str, results: dict) -> None:
    """Write or overwrite the human-readable .txt summary."""
    timestamp = results["metadata"].get("timestamp", "")
    summary_path = output_path.replace(".json", "_summary.txt")
    os.makedirs(os.path.dirname(os.path.abspath(summary_path)), exist_ok=True)
    with open(summary_path, "w", encoding="utf-8") as f:
        f.write(f"LLM Filesystem Survey (Full Sweep) — {timestamp}\n")
        f.write(f"{'=' * 70}\n\n")
        responses = results.get("responses", [])
        successes = sum(1 for r in responses if not r.get("error"))
        f.write(
            f"Total models attempted: {len(responses)}  "
            f"Succeeded: {successes}  "
            f"Failed: {len(responses) - successes}\n\n"
        )
        f.write(f"{'=' * 70}\n\n")
        for entry in responses:
            f.write(f"MODEL: {entry['model']}\n")
            f.write(f"{'─' * 70}\n")
            if entry.get("error"):
                f.write(f"ERROR: {entry['error']}\n\n")
            else:
                elapsed = entry.get("elapsed_seconds", "?")
                words = len((entry.get("response_text") or "").split())
                f.write(f"[elapsed: {elapsed}s, words: {words}]\n\n")
                f.write(f"{entry['response_text']}\n\n")
            f.write("\n")
    return summary_path


# ---------------------------------------------------------------------------
# Main
# ---------------------------------------------------------------------------

def main():
    parser = argparse.ArgumentParser(
        description="LLM Filesystem Survey — full OpenRouter model sweep"
    )
    parser.add_argument(
        "--output",
        type=str,
        default="scripts/survey_sweep_results.json",
        help="Output JSON file (default: scripts/survey_sweep_results.json)",
    )
    parser.add_argument(
        "--delay",
        type=float,
        default=2.0,
        help="Seconds to pause between models (default: 2)",
    )
    parser.add_argument(
        "--timeout",
        type=int,
        default=120,
        help="Per-model HTTP timeout in seconds (default: 120)",
    )
    parser.add_argument(
        "--max-tokens",
        type=int,
        default=4096,
        help="Max response tokens per model (default: 4096)",
    )
    parser.add_argument(
        "--retry-wait",
        type=int,
        default=30,
        help="Seconds to wait on HTTP 429 before retry (default: 30)",
    )
    parser.add_argument(
        "--max-retries",
        type=int,
        default=3,
        help="Max retries on HTTP 429 per model (default: 3)",
    )
    parser.add_argument(
        "--skip-existing",
        action="store_true",
        help="Skip models already present in the output JSON (for resuming)",
    )
    parser.add_argument(
        "--dry-run",
        action="store_true",
        help="Fetch and print the model list without sending any queries",
    )
    args = parser.parse_args()

    api_key = os.environ.get("OPENROUTER_API_KEY")
    if not api_key:
        print("Error: OPENROUTER_API_KEY environment variable not set", file=sys.stderr)
        sys.exit(1)

    timestamp = datetime.now(timezone.utc).strftime("%Y%m%d_%H%M%S")

    # ------------------------------------------------------------------
    # Fetch model list
    # ------------------------------------------------------------------
    print("Fetching model list from OpenRouter...", flush=True)
    all_models = fetch_all_models(api_key)
    print(f"  Total models returned by API: {len(all_models)}")

    chat_models = [m for m in all_models if is_chat_model(m)]
    chat_models.sort(key=lambda m: m["id"])
    print(f"  Chat/text models after filtering: {len(chat_models)}")
    print()

    if args.dry_run:
        print("=== DRY RUN — no queries will be sent ===\n")
        print(f"System prompt:\n{SYSTEM_PROMPT}\n")
        print(f"User message:\n{USER_MESSAGE}\n")
        print(f"Models ({len(chat_models)}):")
        for m in chat_models:
            print(f"  - {m['id']}")
        print(f"\nOutput would go to: {args.output}")
        return

    # ------------------------------------------------------------------
    # Load existing results (for --skip-existing / resumability)
    # ------------------------------------------------------------------
    results: dict
    already_done: set[str] = set()

    output_path = args.output
    if args.skip_existing and os.path.exists(output_path):
        try:
            with open(output_path, "r", encoding="utf-8") as f:
                results = json.load(f)
            already_done = {r["model"] for r in results.get("responses", [])}
            print(
                f"Resuming: found {len(already_done)} already-completed model(s) "
                f"in {output_path}"
            )
        except Exception as e:
            print(f"Warning: could not load existing results ({e}); starting fresh.")
            results = {
                "metadata": {
                    "timestamp": timestamp,
                    "system_prompt": SYSTEM_PROMPT,
                    "user_message": USER_MESSAGE,
                    "questions": QUESTIONS,
                },
                "responses": [],
            }
    else:
        results = {
            "metadata": {
                "timestamp": timestamp,
                "system_prompt": SYSTEM_PROMPT,
                "user_message": USER_MESSAGE,
                "questions": QUESTIONS,
            },
            "responses": [],
        }

    # ------------------------------------------------------------------
    # Decide which models to query
    # ------------------------------------------------------------------
    to_query = [m for m in chat_models if m["id"] not in already_done]

    print(f"=== LLM Filesystem Survey — Full Sweep ===")
    print(f"  Total chat models discovered: {len(chat_models)}")
    print(f"  Already done (skipping):      {len(already_done)}")
    print(f"  Models to query this run:     {len(to_query)}")
    print(f"  Delay between models:         {args.delay}s")
    print(f"  Per-model timeout:            {args.timeout}s")
    print(f"  Max tokens per response:      {args.max_tokens}")
    print(f"  Output:                       {output_path}")
    print()

    # ------------------------------------------------------------------
    # Query each model
    # ------------------------------------------------------------------
    for i, model in enumerate(to_query):
        model_id = model["id"]
        overall_index = len(already_done) + i + 1
        total = len(chat_models)
        print(
            f"  [{overall_index}/{total}] {model_id}...",
            end=" ",
            flush=True,
        )

        t0 = time.time()
        api_resp = query_model(
            model_id,
            api_key,
            timeout=args.timeout,
            max_tokens=args.max_tokens,
            max_retries=args.max_retries,
            retry_wait=args.retry_wait,
        )
        elapsed = time.time() - t0

        response_text = extract_response_text(api_resp)
        error = api_resp.get("error")

        if error:
            print(f"ERROR ({elapsed:.1f}s): {error[:120]}")
        else:
            word_count = len(response_text.split())
            print(f"OK ({elapsed:.1f}s, {word_count} words)")

        entry = {
            "model": model_id,
            "model_name": model.get("name", ""),
            "elapsed_seconds": round(elapsed, 1),
            "response_text": response_text,
            "error": error,
            "usage": api_resp.get("usage"),
        }
        results["responses"].append(entry)

        # Write incrementally so a crash doesn't lose earlier results.
        write_results(output_path, results)

        if i < len(to_query) - 1 and args.delay > 0:
            time.sleep(args.delay)

    # ------------------------------------------------------------------
    # Final summary
    # ------------------------------------------------------------------
    print()
    all_responses = results["responses"]
    successes = sum(1 for r in all_responses if not r.get("error"))
    failures = len(all_responses) - successes
    print(f"=== Done: {successes} succeeded, {failures} failed ===")
    print(f"  Results JSON: {output_path}")

    summary_path = write_summary(output_path, results)
    print(f"  Summary TXT:  {summary_path}")


if __name__ == "__main__":
    main()
