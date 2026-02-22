#!/usr/bin/env python3
"""
LLM Filesystem Experience Survey
=================================
Sends a structured questionnaire to multiple LLM families via OpenRouter
to understand how AI agents experience filesystem interaction.

Uses only Python stdlib (no requests dependency).

Usage:
    OPENROUTER_API_KEY=... python3 scripts/llm_fs_survey.py
    python3 scripts/llm_fs_survey.py --models "openai/gpt-4o,google/gemini-2.5-flash"
    python3 scripts/llm_fs_survey.py --output results/survey_run1.json
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
# Survey content
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
# Default models — one from each major family
# ---------------------------------------------------------------------------

DEFAULT_MODELS = [
    "anthropic/claude-sonnet-4",
    "anthropic/claude-haiku-4",
    "openai/gpt-4o",
    "openai/gpt-4.1-mini",
    "google/gemini-2.5-flash",
    "google/gemini-2.5-pro",
    "meta-llama/llama-4-maverick",
    "deepseek/deepseek-chat-v3-0324",
]

# ---------------------------------------------------------------------------
# OpenRouter API
# ---------------------------------------------------------------------------

OPENROUTER_URL = "https://openrouter.ai/api/v1/chat/completions"


def query_model(model_id: str, api_key: str, timeout: int = 120,
                max_tokens: int = 4096) -> dict:
    """Send the survey to a single model. Returns raw API response dict."""
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
        "HTTP-Referer": "https://github.com/fsgeek/assfs",
        "X-Title": "AssFS LLM Filesystem Survey",
    }

    data = json.dumps(payload).encode("utf-8")
    req = urllib.request.Request(
        OPENROUTER_URL, data=data, headers=headers, method="POST"
    )

    try:
        with urllib.request.urlopen(req, timeout=timeout) as resp:
            return json.loads(resp.read().decode("utf-8"))
    except urllib.error.HTTPError as e:
        body = e.read().decode("utf-8", errors="replace")
        return {"error": f"HTTP {e.code}: {body}", "model": model_id}
    except Exception as e:
        return {"error": str(e), "model": model_id}


def extract_response_text(api_response: dict) -> str:
    """Pull the assistant message text from an OpenRouter response."""
    try:
        return api_response["choices"][0]["message"]["content"]
    except (KeyError, IndexError):
        return ""


# ---------------------------------------------------------------------------
# Main
# ---------------------------------------------------------------------------

def main():
    parser = argparse.ArgumentParser(
        description="LLM Filesystem Experience Survey via OpenRouter"
    )
    parser.add_argument(
        "--models",
        type=str,
        default=None,
        help="Comma-separated model IDs (default: all major families)",
    )
    parser.add_argument(
        "--output",
        type=str,
        default=None,
        help="Output JSON file (default: scripts/survey_results_<timestamp>.json)",
    )
    parser.add_argument(
        "--timeout",
        type=int,
        default=120,
        help="Per-model timeout in seconds (default: 120)",
    )
    parser.add_argument(
        "--max-tokens",
        type=int,
        default=4096,
        help="Max response tokens per model (default: 4096)",
    )
    parser.add_argument(
        "--dry-run",
        action="store_true",
        help="Print the prompt and model list without querying",
    )
    args = parser.parse_args()

    api_key = os.environ.get("OPENROUTER_API_KEY")
    if not api_key and not args.dry_run:
        print("Error: OPENROUTER_API_KEY not set", file=sys.stderr)
        sys.exit(1)

    models = (
        [m.strip() for m in args.models.split(",")]
        if args.models
        else DEFAULT_MODELS
    )

    timestamp = datetime.now(timezone.utc).strftime("%Y%m%d_%H%M%S")
    output_path = args.output or f"scripts/survey_results_{timestamp}.json"

    if args.dry_run:
        print("=== DRY RUN ===")
        print(f"\nSystem prompt:\n{SYSTEM_PROMPT}\n")
        print(f"User message:\n{USER_MESSAGE}\n")
        print(f"Models ({len(models)}):")
        for m in models:
            print(f"  - {m}")
        print(f"\nOutput would go to: {output_path}")
        return

    print(f"=== LLM Filesystem Survey ===")
    print(f"  Models: {len(models)}")
    print(f"  Output: {output_path}")
    print(f"  Timeout: {args.timeout}s per model")
    print()

    results = {
        "metadata": {
            "timestamp": timestamp,
            "system_prompt": SYSTEM_PROMPT,
            "user_message": USER_MESSAGE,
            "questions": QUESTIONS,
        },
        "responses": [],
    }

    for i, model_id in enumerate(models):
        print(f"  [{i+1}/{len(models)}] {model_id}...", end=" ", flush=True)
        t0 = time.time()
        api_resp = query_model(model_id, api_key, timeout=args.timeout,
                               max_tokens=args.max_tokens)
        elapsed = time.time() - t0

        response_text = extract_response_text(api_resp)
        error = api_resp.get("error")

        if error:
            print(f"ERROR ({elapsed:.1f}s): {error[:100]}")
        else:
            word_count = len(response_text.split())
            print(f"OK ({elapsed:.1f}s, {word_count} words)")

        entry = {
            "model": model_id,
            "elapsed_seconds": round(elapsed, 1),
            "response_text": response_text,
            "error": error,
            "usage": api_resp.get("usage"),
        }
        results["responses"].append(entry)

        # Write incrementally so we don't lose results on crash.
        os.makedirs(os.path.dirname(output_path) or ".", exist_ok=True)
        with open(output_path, "w") as f:
            json.dump(results, f, indent=2)

    # Summary.
    print()
    successes = sum(1 for r in results["responses"] if not r["error"])
    failures = len(results["responses"]) - successes
    print(f"=== Done: {successes} succeeded, {failures} failed ===")
    print(f"  Results: {output_path}")

    # Also write a human-readable summary.
    summary_path = output_path.replace(".json", "_summary.txt")
    with open(summary_path, "w") as f:
        f.write(f"LLM Filesystem Survey — {timestamp}\n")
        f.write(f"{'=' * 60}\n\n")
        for entry in results["responses"]:
            f.write(f"MODEL: {entry['model']}\n")
            f.write(f"{'─' * 60}\n")
            if entry["error"]:
                f.write(f"ERROR: {entry['error']}\n\n")
            else:
                f.write(f"{entry['response_text']}\n\n")
            f.write("\n")
    print(f"  Summary: {summary_path}")


if __name__ == "__main__":
    main()
