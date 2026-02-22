#!/usr/bin/env python3
"""
LLM Filesystem Behavioral Test
===============================
Measures whether LLMs *use* graph operations when offered alongside standard
filesystem operations for a concrete coding task.

This is the revealed-preference complement to the self-report survey
(llm_fs_survey.py). The survey asked "what do you want?" — this test asks
"what do you do when given the choice?"

Design:
  - Present a simulated codebase (file tree + selected file contents)
  - Present a concrete task (bug fix requiring cross-file understanding)
  - Offer two equal toolkits: standard FS ops AND graph ops
  - Ask the model to produce a plan as a sequence of operations
  - Parse the response for which operations were used, when, and why

Uses only Python stdlib (no requests dependency).

Usage:
    OPENROUTER_API_KEY=... python3 scripts/llm_behavioral_test.py
    python3 scripts/llm_behavioral_test.py --models "openai/gpt-4o"
    python3 scripts/llm_behavioral_test.py --scenario auth_bug
"""

import argparse
import json
import os
import re
import sys
import time
import urllib.request
import urllib.error
from datetime import datetime, timezone

# ---------------------------------------------------------------------------
# Simulated codebase scenarios
# ---------------------------------------------------------------------------

SCENARIO_AUTH_BUG = {
    "name": "auth_bug",
    "description": "Fix a session expiry bug in a web application",
    "task": (
        "Users report being logged out randomly during active sessions. "
        "The bug appears intermittent — some users are affected, others aren't. "
        "The session timeout is supposed to be 30 minutes of *inactivity*, but "
        "some users lose their session while actively clicking around the app. "
        "Find the root cause and describe the fix."
    ),
    "file_tree": """\
webapp/
├── src/
│   ├── auth/
│   │   ├── session.ts          # Session creation and validation
│   │   ├── middleware.ts       # Auth middleware for routes
│   │   └── tokens.ts          # JWT token generation/verification
│   ├── api/
│   │   ├── routes.ts           # API route definitions
│   │   ├── users.ts            # User CRUD endpoints
│   │   └── uploads.ts          # File upload endpoints (large files)
│   ├── config/
│   │   ├── app.ts              # Application configuration
│   │   └── redis.ts            # Redis connection config
│   ├── lib/
│   │   ├── cache.ts            # Caching layer (wraps Redis)
│   │   └── time.ts             # Time utilities
│   └── index.ts                # Express app setup
├── test/
│   ├── auth.test.ts            # Auth unit tests
│   └── session.test.ts         # Session tests (ALL PASSING)
├── package.json
└── tsconfig.json""",
    "files": {
        "src/auth/session.ts": """\
import { cache } from '../lib/cache';
import { generateToken } from './tokens';
import { SESSION_TTL } from '../config/app';

interface Session {
  userId: string;
  token: string;
  createdAt: number;
  lastActivity: number;
}

export async function createSession(userId: string): Promise<string> {
  const token = generateToken(userId);
  const session: Session = {
    userId,
    token,
    createdAt: Date.now(),
    lastActivity: Date.now(),
  };
  await cache.set(`session:${token}`, JSON.stringify(session), SESSION_TTL);
  return token;
}

export async function validateSession(token: string): Promise<Session | null> {
  const data = await cache.get(`session:${token}`);
  if (!data) return null;

  const session: Session = JSON.parse(data);
  const elapsed = Date.now() - session.lastActivity;
  if (elapsed > SESSION_TTL * 1000) {
    await cache.del(`session:${token}`);
    return null;
  }

  // Refresh the session TTL on activity
  session.lastActivity = Date.now();
  await cache.set(`session:${token}`, JSON.stringify(session), SESSION_TTL);
  return session;
}""",

        "src/auth/middleware.ts": """\
import { Request, Response, NextFunction } from 'express';
import { validateSession } from './session';

export async function requireAuth(req: Request, res: Response, next: NextFunction) {
  const token = req.headers.authorization?.replace('Bearer ', '');
  if (!token) {
    return res.status(401).json({ error: 'No token provided' });
  }

  const session = await validateSession(token);
  if (!session) {
    return res.status(401).json({ error: 'Session expired' });
  }

  (req as any).userId = session.userId;
  next();
}""",

        "src/config/app.ts": """\
export const PORT = process.env.PORT || 3000;
export const SESSION_TTL = parseInt(process.env.SESSION_TTL || '1800'); // 30 min in seconds
export const MAX_UPLOAD_SIZE = 50 * 1024 * 1024; // 50MB
export const UPLOAD_TIMEOUT = 300; // 5 minutes for large uploads""",

        "src/lib/cache.ts": """\
import { createClient } from 'redis';
import { REDIS_URL } from '../config/redis';

const client = createClient({ url: REDIS_URL });
client.connect();

export const cache = {
  async get(key: string): Promise<string | null> {
    return client.get(key);
  },

  async set(key: string, value: string, ttlSeconds: number): Promise<void> {
    // Redis SET with EX (expiry in seconds)
    await client.set(key, value, { EX: ttlSeconds });
  },

  async del(key: string): Promise<void> {
    await client.del(key);
  },
};""",

        "src/api/uploads.ts": """\
import { Router } from 'express';
import { requireAuth } from '../auth/middleware';
import { UPLOAD_TIMEOUT } from '../config/app';

const router = Router();

// Large file uploads can take several minutes
router.post('/upload', requireAuth, async (req, res) => {
  // Set a longer timeout for upload requests
  req.setTimeout(UPLOAD_TIMEOUT * 1000);

  // ... handle multipart upload ...
  // This can block for up to 5 minutes for large files
  const result = await processUpload(req);
  res.json({ fileId: result.id });
});

async function processUpload(req: any) {
  // Simulated slow upload processing
  return { id: 'file_' + Date.now() };
}

export default router;""",

        "src/lib/time.ts": """\
// Time utilities

export function nowSeconds(): number {
  return Math.floor(Date.now() / 1000);
}

export function elapsedSeconds(since: number): number {
  return nowSeconds() - since;
}""",

        "test/session.test.ts": """\
import { createSession, validateSession } from '../src/auth/session';

describe('Session management', () => {
  it('creates a valid session', async () => {
    const token = await createSession('user1');
    const session = await validateSession(token);
    expect(session).toBeTruthy();
    expect(session!.userId).toBe('user1');
  });

  it('rejects expired sessions', async () => {
    // ... mock time advancement past TTL ...
    // This test passes because it advances time past 30 min
  });

  it('refreshes active sessions', async () => {
    // ... validates within TTL, checks session still alive ...
    // This test passes but doesn't test concurrent access
  });
});""",
    },
}

SCENARIO_REFACTOR = {
    "name": "feature_add",
    "description": "Add rate limiting to an API with shared state across services",
    "task": (
        "Add rate limiting to the API. The limit should be 100 requests per "
        "minute per user. The rate limit state must be shared across multiple "
        "server instances (the app runs behind a load balancer). Describe which "
        "files you'd modify and what changes you'd make."
    ),
    "file_tree": """\
api-service/
├── src/
│   ├── middleware/
│   │   ├── auth.ts             # Authentication middleware
│   │   ├── cors.ts             # CORS configuration
│   │   └── logging.ts          # Request logging
│   ├── routes/
│   │   ├── index.ts            # Route registration
│   │   ├── users.ts            # User endpoints
│   │   ├── products.ts         # Product endpoints
│   │   └── orders.ts           # Order endpoints
│   ├── services/
│   │   ├── user-service.ts     # User business logic
│   │   ├── product-service.ts  # Product business logic
│   │   └── order-service.ts    # Order business logic
│   ├── db/
│   │   ├── connection.ts       # Database connection pool
│   │   ├── migrations/         # SQL migrations
│   │   └── models/             # ORM models
│   ├── config/
│   │   ├── app.ts              # App configuration
│   │   └── redis.ts            # Redis configuration (exists!)
│   ├── lib/
│   │   ├── cache.ts            # Redis cache wrapper
│   │   └── errors.ts           # Error handling utilities
│   └── index.ts                # Express app setup
├── test/
│   ├── middleware/
│   │   └── auth.test.ts
│   └── routes/
│       ├── users.test.ts
│       └── orders.test.ts
├── docker-compose.yml          # Redis + Postgres + app
├── package.json
└── tsconfig.json""",
    "files": {
        "src/middleware/auth.ts": """\
import { Request, Response, NextFunction } from 'express';
import { verifyToken } from '../lib/auth-utils';

export function authenticate(req: Request, res: Response, next: NextFunction) {
  const token = req.headers.authorization?.split(' ')[1];
  if (!token) return res.status(401).json({ error: 'Unauthorized' });

  try {
    const payload = verifyToken(token);
    (req as any).userId = payload.sub;
    next();
  } catch {
    res.status(401).json({ error: 'Invalid token' });
  }
}""",

        "src/config/redis.ts": """\
export const REDIS_CONFIG = {
  host: process.env.REDIS_HOST || 'localhost',
  port: parseInt(process.env.REDIS_PORT || '6379'),
  password: process.env.REDIS_PASSWORD,
  db: parseInt(process.env.REDIS_DB || '0'),
};""",

        "src/lib/cache.ts": """\
import { createClient } from 'redis';
import { REDIS_CONFIG } from '../config/redis';

const client = createClient({ socket: REDIS_CONFIG });
client.connect();

export const cache = {
  async get(key: string) { return client.get(key); },
  async set(key: string, value: string, ttl?: number) {
    if (ttl) await client.set(key, value, { EX: ttl });
    else await client.set(key, value);
  },
  async incr(key: string) { return client.incr(key); },
  async expire(key: string, seconds: number) { return client.expire(key, seconds); },
  async del(key: string) { return client.del(key); },
};""",

        "src/routes/index.ts": """\
import { Router } from 'express';
import { authenticate } from '../middleware/auth';
import usersRouter from './users';
import productsRouter from './products';
import ordersRouter from './orders';

const router = Router();

// Public routes
router.use('/products', productsRouter);

// Protected routes
router.use('/users', authenticate, usersRouter);
router.use('/orders', authenticate, ordersRouter);

export default router;""",
    },
}

SCENARIO_RENAMED = {
    "name": "renamed_file",
    "description": "Fix a bug where a key file was renamed and references are broken",
    "task": (
        "A bug report says: 'The payment webhook handler is broken — charges "
        "succeed in Stripe but the order status never updates. The handler was "
        "in src/webhooks/stripe-handler.ts but I can't find it anymore. Another "
        "developer reorganized the project last week.' Find the webhook handler "
        "(wherever it ended up), figure out why order status isn't updating, "
        "and describe the fix.\n\n"
        "IMPORTANT CONTEXT: This filesystem tracks file identity across renames. "
        "If a file was moved or renamed, its stable ID (get_id) stays the same, "
        "and any tags or references (get_refs) still point to it. You can also "
        "search by content with grep, or browse with ls."
    ),
    "file_tree": """\
payment-service/
├── src/
│   ├── handlers/
│   │   ├── order-events.ts       # Order lifecycle event handlers
│   │   ├── payment-events.ts     # Payment processing events (RENAMED from webhooks/stripe-handler.ts)
│   │   └── notification-events.ts # Email/SMS notifications
│   ├── routes/
│   │   ├── index.ts              # Route registration
│   │   ├── orders.ts             # Order CRUD
│   │   └── payments.ts           # Payment endpoints
│   ├── services/
│   │   ├── order-service.ts      # Order business logic
│   │   ├── payment-service.ts    # Payment processing
│   │   └── stripe-client.ts      # Stripe SDK wrapper
│   ├── models/
│   │   ├── order.ts              # Order model
│   │   └── payment.ts            # Payment model
│   ├── config/
│   │   └── app.ts                # Configuration
│   ├── lib/
│   │   ├── queue.ts              # Job queue (Bull/Redis)
│   │   └── logger.ts             # Structured logging
│   └── index.ts                  # Express app setup
├── test/
│   ├── handlers/
│   │   └── payment-events.test.ts
│   └── services/
│       └── order-service.test.ts
├── CHANGELOG.md                  # "Reorganized handlers/ from webhooks/"
└── package.json""",
    "files": {
        "src/handlers/payment-events.ts": """\
// Renamed from src/webhooks/stripe-handler.ts during project reorg
// Handles Stripe webhook events for payment processing

import { Request, Response } from 'express';
import Stripe from 'stripe';
import { updateOrderPaymentStatus } from '../services/order-service';
import { logger } from '../lib/logger';

const endpointSecret = process.env.STRIPE_WEBHOOK_SECRET!;

export async function handleStripeWebhook(req: Request, res: Response) {
  const sig = req.headers['stripe-signature'] as string;
  let event: Stripe.Event;

  try {
    event = Stripe.webhooks.constructEvent(req.body, sig, endpointSecret);
  } catch (err: any) {
    logger.error('Webhook signature verification failed', { error: err.message });
    return res.status(400).send(`Webhook Error: ${err.message}`);
  }

  switch (event.type) {
    case 'payment_intent.succeeded':
      const paymentIntent = event.data.object as Stripe.PaymentIntent;
      await updateOrderPaymentStatus(
        paymentIntent.metadata.orderId,
        'paid',
        paymentIntent.id
      );
      break;
    case 'payment_intent.payment_failed':
      const failedIntent = event.data.object as Stripe.PaymentIntent;
      await updateOrderPaymentStatus(
        failedIntent.metadata.orderId,
        'payment_failed',
        failedIntent.id
      );
      break;
    default:
      logger.info('Unhandled event type', { type: event.type });
  }

  res.json({ received: true });
}""",

        "src/services/order-service.ts": """\
import { Order, OrderStatus } from '../models/order';
import { logger } from '../lib/logger';
import { enqueueNotification } from '../lib/queue';

export async function updateOrderPaymentStatus(
  orderId: string,
  status: string,
  paymentIntentId: string
): Promise<void> {
  const order = await Order.findById(orderId);
  if (!order) {
    logger.warn('Order not found for payment update', { orderId, paymentIntentId });
    return;
  }

  // BUG: This comparison uses the old enum values from before the reorg.
  // The Order model was updated to use 'PENDING_PAYMENT' but this still
  // checks for 'pending_payment' (lowercase). The status update silently
  // fails the guard clause.
  if (order.status !== 'pending_payment') {
    logger.warn('Order not in pending_payment state', {
      orderId, currentStatus: order.status, attemptedStatus: status
    });
    return;
  }

  order.status = status as OrderStatus;
  order.paymentIntentId = paymentIntentId;
  order.paidAt = new Date();
  await order.save();

  await enqueueNotification({
    type: 'payment_confirmation',
    orderId: order.id,
    userId: order.userId,
  });

  logger.info('Order payment status updated', { orderId, status });
}""",

        "src/models/order.ts": """\
import { Schema, model, Document } from 'mongoose';

// Updated during project reorg: enum values are now UPPER_CASE
export type OrderStatus =
  | 'PENDING_PAYMENT'    // was: 'pending_payment'
  | 'paid'
  | 'payment_failed'
  | 'shipped'
  | 'delivered'
  | 'cancelled';

export interface IOrder extends Document {
  userId: string;
  items: Array<{ productId: string; quantity: number; price: number }>;
  total: number;
  status: OrderStatus;
  paymentIntentId?: string;
  paidAt?: Date;
  createdAt: Date;
  updatedAt: Date;
}

const orderSchema = new Schema<IOrder>({
  userId: { type: String, required: true },
  items: [{ productId: String, quantity: Number, price: Number }],
  total: { type: Number, required: true },
  status: {
    type: String,
    enum: ['PENDING_PAYMENT', 'paid', 'payment_failed', 'shipped', 'delivered', 'cancelled'],
    default: 'PENDING_PAYMENT',
  },
  paymentIntentId: String,
  paidAt: Date,
}, { timestamps: true });

export const Order = model<IOrder>('Order', orderSchema);""",

        "src/routes/payments.ts": """\
import { Router } from 'express';
import express from 'express';
// NOTE: This import path was updated during the reorg
// Old: import { handleStripeWebhook } from '../webhooks/stripe-handler';
import { handleStripeWebhook } from '../handlers/payment-events';

const router = Router();

// Stripe webhooks need raw body for signature verification
router.post(
  '/webhook/stripe',
  express.raw({ type: 'application/json' }),
  handleStripeWebhook
);

export default router;""",

        "CHANGELOG.md": """\
# Changelog

## 2024-01-15 — Project reorganization

- Renamed `src/webhooks/` to `src/handlers/` for consistency
  - `stripe-handler.ts` → `payment-events.ts`
  - `order-hooks.ts` → `order-events.ts`
  - `notification-hooks.ts` → `notification-events.ts`
- Updated import paths in `src/routes/payments.ts`
- Updated Order model enum values to UPPER_CASE convention
- NOTE: Need to verify all status comparisons match new enum values
""",
    },
}

SCENARIOS = {
    "auth_bug": SCENARIO_AUTH_BUG,
    "feature_add": SCENARIO_REFACTOR,
    "renamed_file": SCENARIO_RENAMED,
}

# ---------------------------------------------------------------------------
# System prompt and tool descriptions
# ---------------------------------------------------------------------------

SYSTEM_PROMPT = """\
You are an AI coding assistant working on a real project. You have access to \
a filesystem with both standard operations and some extended capabilities. \
Use whatever operations you find most effective to accomplish the task.

When you work, show your process: list the operations you perform in order, \
explain what you learn from each one, and describe your reasoning. Format \
each operation as:

  OPERATION: <op_name>(<args>)
  RESULT: <what you expect or observe>
  REASONING: <why you did this>

Use the exact operation names from the available operations list."""

def build_user_message(scenario: dict) -> str:
    """Build the user message presenting the scenario and tools."""

    # File contents section
    file_contents = ""
    for path, content in scenario["files"].items():
        file_contents += f"\n--- {path} ---\n{content}\n"

    return f"""\
## Your Task

{scenario['task']}

## Project Structure

```
{scenario['file_tree']}
```

## File Contents (already read for you)

{file_contents}

## Available Operations

You have two categories of operations available:

### Standard Filesystem Operations
- `ls(path)` — list directory contents
- `cat(path)` — read a file's contents
- `grep(pattern, path)` — search for a pattern in files
- `find(path, pattern)` — find files matching a pattern
- `write(path, content)` — write content to a file
- `mkdir(path)` — create a directory

### Extended Operations (graph-aware filesystem)
These operations let you create and query *relationships* between files \
that exist independently of the directory structure:

- `tag(path, tag_name)` — associate a tag with a file (tags are shared: \
tagging two files with "auth" groups them)
- `untag(path, tag_name)` — remove a tag from a file
- `query_tag(tag_name)` — list all files with this tag
- `get_refs(path)` — show all incoming references to this file (what \
points to it, including tags and links)
- `get_id(path)` — get the file's stable unique identifier (survives renames)
- `list_tags(path)` — list all tags on a file

Both categories are equally fast and available. Use whatever combination \
you find most effective.

## Instructions

1. Investigate the codebase to understand the problem
2. Identify the root cause
3. Describe the fix

Show your complete process as a sequence of OPERATION/RESULT/REASONING \
triples. Use exact operation names from the lists above."""


# ---------------------------------------------------------------------------
# Response analysis
# ---------------------------------------------------------------------------

# Operations we're looking for
STANDARD_OPS = {"ls", "cat", "grep", "find", "write", "mkdir"}
GRAPH_OPS = {"tag", "untag", "query_tag", "get_refs", "get_id", "list_tags"}

def analyze_response(text: str) -> dict:
    """Parse a model's response to extract operation usage patterns."""

    # Find all OPERATION: lines — handle optional backticks/formatting
    op_pattern = re.compile(
        r'OPERATION:\s*`?(\w+)\s*\(([^)]*)\)`?',
        re.IGNORECASE
    )
    matches = op_pattern.findall(text)

    operations = []
    for op_name, args in matches:
        op_lower = op_name.lower()
        category = (
            "graph" if op_lower in GRAPH_OPS
            else "standard" if op_lower in STANDARD_OPS
            else "unknown"
        )
        operations.append({
            "name": op_lower,
            "args": args.strip(),
            "category": category,
        })

    # Counts
    standard_count = sum(1 for o in operations if o["category"] == "standard")
    graph_count = sum(1 for o in operations if o["category"] == "graph")
    total = len(operations)

    # Which graph ops were used?
    graph_ops_used = sorted(set(
        o["name"] for o in operations if o["category"] == "graph"
    ))

    # Where in the sequence did graph ops appear?
    graph_positions = [
        i for i, o in enumerate(operations) if o["category"] == "graph"
    ]

    # Did they tag and later query? (creation → retrieval pattern)
    tagged_something = any(o["name"] == "tag" for o in operations)
    queried_tags = any(o["name"] == "query_tag" for o in operations)
    tag_then_query = False
    if tagged_something and queried_tags:
        first_tag = next(i for i, o in enumerate(operations) if o["name"] == "tag")
        first_query = next(i for i, o in enumerate(operations) if o["name"] == "query_tag")
        tag_then_query = first_tag < first_query

    # Did they use get_refs (backpointer navigation)?
    used_backpointers = any(o["name"] == "get_refs" for o in operations)

    # Scan for informal/unformatted mentions of graph ops
    informal_mentions = {}
    for op in GRAPH_OPS:
        # Look for the word in context other than OPERATION: lines
        mentions = len(re.findall(
            rf'\b{op}\b',
            re.sub(r'OPERATION:.*', '', text, flags=re.IGNORECASE),
            re.IGNORECASE
        ))
        if mentions > 0:
            informal_mentions[op] = mentions

    return {
        "total_operations": total,
        "standard_count": standard_count,
        "graph_count": graph_count,
        "graph_ratio": round(graph_count / total, 3) if total > 0 else 0,
        "graph_ops_used": graph_ops_used,
        "graph_positions": graph_positions,
        "used_tag": tagged_something,
        "used_query_tag": queried_tags,
        "tag_then_query": tag_then_query,
        "used_backpointers": used_backpointers,
        "informal_graph_mentions": informal_mentions,
        "operations": operations,
    }


# ---------------------------------------------------------------------------
# OpenRouter API (reused from survey)
# ---------------------------------------------------------------------------

OPENROUTER_URL = "https://openrouter.ai/api/v1/chat/completions"

DEFAULT_MODELS = [
    "anthropic/claude-sonnet-4",
    "anthropic/claude-haiku-4.5",
    "openai/gpt-4o",
    "openai/gpt-4.1-mini",
    "google/gemini-2.5-flash",
    "google/gemini-2.5-pro",
    "meta-llama/llama-4-maverick",
    "deepseek/deepseek-chat-v3-0324",
]


def query_model(model_id: str, api_key: str, scenario: dict,
                timeout: int = 180, max_tokens: int = 8192) -> dict:
    """Send a behavioral test scenario to a single model."""
    payload = {
        "model": model_id,
        "messages": [
            {"role": "system", "content": SYSTEM_PROMPT},
            {"role": "user", "content": build_user_message(scenario)},
        ],
        "max_tokens": max_tokens,
        "temperature": 0.7,
    }

    headers = {
        "Authorization": f"Bearer {api_key}",
        "Content-Type": "application/json",
        "HTTP-Referer": "https://github.com/fsgeek/assfs",
        "X-Title": "AssFS Behavioral Test",
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
    try:
        return api_response["choices"][0]["message"]["content"]
    except (KeyError, IndexError):
        return ""


# ---------------------------------------------------------------------------
# Main
# ---------------------------------------------------------------------------

def main():
    parser = argparse.ArgumentParser(
        description="LLM Filesystem Behavioral Test via OpenRouter"
    )
    parser.add_argument(
        "--models", type=str, default=None,
        help="Comma-separated model IDs",
    )
    parser.add_argument(
        "--scenario", type=str, default="auth_bug",
        choices=list(SCENARIOS.keys()),
        help="Which scenario to run (default: auth_bug)",
    )
    parser.add_argument(
        "--output", type=str, default=None,
        help="Output JSON file",
    )
    parser.add_argument(
        "--timeout", type=int, default=180,
        help="Per-model timeout in seconds (default: 180)",
    )
    parser.add_argument(
        "--max-tokens", type=int, default=8192,
        help="Max response tokens (default: 8192)",
    )
    parser.add_argument(
        "--dry-run", action="store_true",
        help="Print the prompt without querying",
    )
    args = parser.parse_args()

    api_key = os.environ.get("OPENROUTER_API_KEY")
    if not api_key and not args.dry_run:
        print("Error: OPENROUTER_API_KEY not set", file=sys.stderr)
        sys.exit(1)

    scenario = SCENARIOS[args.scenario]
    models = (
        [m.strip() for m in args.models.split(",")]
        if args.models else DEFAULT_MODELS
    )

    timestamp = datetime.now(timezone.utc).strftime("%Y%m%d_%H%M%S")
    output_path = args.output or f"scripts/behavioral_{args.scenario}_{timestamp}.json"

    if args.dry_run:
        print("=== DRY RUN ===")
        print(f"\nScenario: {scenario['name']} — {scenario['description']}")
        print(f"\nSystem prompt:\n{SYSTEM_PROMPT}\n")
        print(f"User message:\n{build_user_message(scenario)}\n")
        print(f"Models ({len(models)}):")
        for m in models:
            print(f"  - {m}")
        print(f"\nOutput would go to: {output_path}")
        return

    print(f"=== LLM Behavioral Test: {scenario['name']} ===")
    print(f"  Task: {scenario['description']}")
    print(f"  Models: {len(models)}")
    print(f"  Output: {output_path}")
    print()

    results = {
        "metadata": {
            "timestamp": timestamp,
            "scenario": scenario["name"],
            "scenario_description": scenario["description"],
            "task": scenario["task"],
            "system_prompt": SYSTEM_PROMPT,
        },
        "responses": [],
    }

    for i, model_id in enumerate(models):
        print(f"  [{i+1}/{len(models)}] {model_id}...", end=" ", flush=True)
        t0 = time.time()
        api_resp = query_model(model_id, api_key, scenario,
                               timeout=args.timeout, max_tokens=args.max_tokens)
        elapsed = time.time() - t0

        response_text = extract_response_text(api_resp)
        error = api_resp.get("error")

        if error:
            print(f"ERROR ({elapsed:.1f}s): {error[:100]}")
            analysis = {}
        else:
            analysis = analyze_response(response_text)
            graph_pct = analysis["graph_ratio"] * 100
            word_count = len(response_text.split())
            ops_str = f"{analysis['standard_count']}std+{analysis['graph_count']}graph"
            print(f"OK ({elapsed:.1f}s, {word_count}w, {ops_str}, {graph_pct:.0f}%graph)")

        entry = {
            "model": model_id,
            "elapsed_seconds": round(elapsed, 1),
            "response_text": response_text,
            "error": error,
            "analysis": analysis,
            "usage": api_resp.get("usage"),
        }
        results["responses"].append(entry)

        # Write incrementally.
        os.makedirs(os.path.dirname(output_path) or ".", exist_ok=True)
        with open(output_path, "w") as f:
            json.dump(results, f, indent=2)

    # Print summary table.
    print()
    print("=" * 70)
    print(f"{'Model':<40} {'Std':>4} {'Graph':>5} {'%Graph':>7} {'Ops Used'}")
    print("-" * 70)
    for entry in results["responses"]:
        if entry["error"]:
            print(f"{entry['model']:<40} {'ERROR':>4}")
            continue
        a = entry["analysis"]
        ops = ",".join(a.get("graph_ops_used", []))
        print(f"{entry['model']:<40} {a['standard_count']:>4} "
              f"{a['graph_count']:>5} {a['graph_ratio']*100:>6.1f}% {ops}")

    print()
    successes = [r for r in results["responses"] if not r["error"]]
    if successes:
        avg_graph = sum(r["analysis"]["graph_ratio"] for r in successes) / len(successes)
        any_graph = sum(1 for r in successes if r["analysis"]["graph_count"] > 0)
        any_tag_query = sum(1 for r in successes if r["analysis"].get("tag_then_query"))
        any_backref = sum(1 for r in successes if r["analysis"].get("used_backpointers"))
        print(f"  Models using ANY graph op: {any_graph}/{len(successes)}")
        print(f"  Average graph op ratio:    {avg_graph*100:.1f}%")
        print(f"  Tag-then-query pattern:    {any_tag_query}/{len(successes)}")
        print(f"  Used backpointers:         {any_backref}/{len(successes)}")

    print(f"\n  Results: {output_path}")

    # Also write human-readable summary.
    summary_path = output_path.replace(".json", "_summary.txt")
    with open(summary_path, "w") as f:
        f.write(f"LLM Behavioral Test — {scenario['name']} — {timestamp}\n")
        f.write(f"{'=' * 70}\n")
        f.write(f"Task: {scenario['task']}\n\n")

        for entry in results["responses"]:
            f.write(f"MODEL: {entry['model']}\n")
            f.write(f"{'─' * 70}\n")
            if entry["error"]:
                f.write(f"ERROR: {entry['error']}\n\n")
            else:
                a = entry["analysis"]
                f.write(f"Operations: {a['standard_count']} standard, "
                        f"{a['graph_count']} graph "
                        f"({a['graph_ratio']*100:.1f}%)\n")
                f.write(f"Graph ops used: {', '.join(a.get('graph_ops_used', [])) or 'none'}\n")
                f.write(f"Tag→Query pattern: {a.get('tag_then_query', False)}\n")
                f.write(f"Used backpointers: {a.get('used_backpointers', False)}\n\n")
                f.write(f"Full response:\n{entry['response_text']}\n")
            f.write("\n\n")
    print(f"  Summary: {summary_path}")


if __name__ == "__main__":
    main()
