# Lumen AI Chat

Lumen is a Vue 3 + Rust/Axum chat application that invokes real AI coding agents and also supports direct hosted-model APIs. Selections use the canonical route `<aiCoder>/<provider>/<model>`, for example `hermes/z.ai/glm-5.2` or `codex/codex/gpt-5.6`.

It ships an [agent harness](#agent-harness): a provider-neutral tool loop that reads and edits the workspace, searches an indexed codebase, and works the same way over every hosted provider protocol — delegating to a native coder CLI when that CLI is the agent.

## AI coder support

Lumen detects these native CLIs on `PATH` and executes the selected program—not a relabeled generic HTTP request:

- Hermes: `hermes chat`
- OpenCode: `opencode run --format json`
- KiloCode: `kilocode run --format json`
- Claude Code: `claude --print --output-format json`
- OpenAI Codex: `codex exec --json`

When `AI_CODER_ROUTES_JSON` is unset, each installed CLI receives a default route:

- `hermes/auto/default`
- `opencode/auto/default`
- `kilocode/auto/default`
- `claude/claude/default`
- `codex/codex/default`

Coders that support multiple providers use their provider as the middle segment. Fixed-provider coders repeat their coder name as the provider segment.

Configure explicit routes and display names when you want model-level selection:

```bash
export AI_CODER_ROUTES_JSON='[
  {
    "coder": "hermes",
    "provider": "z.ai",
    "provider_name": "Z.AI",
    "model": "glm-5.2",
    "model_name": "GLM 5.2"
  },
  {
    "coder": "opencode",
    "provider": "openrouter",
    "model": "anthropic/claude-sonnet-4"
  },
  {
    "coder": "kilocode",
    "provider": "kilocode",
    "model": "kilo-model"
  },
  {
    "coder": "claude",
    "provider": "claude",
    "model": "sonnet"
  },
  {
    "coder": "codex",
    "provider": "codex",
    "model": "gpt-5.6"
  }
]'
```

The default selection is the first configured `hermes` route. Override it with `AI_DEFAULT_CODER` and `AI_DEFAULT_PROVIDER`.

### Native authentication

Authenticate each CLI using its own supported login flow before selecting it:

```bash
hermes auth add <provider>   # or: hermes setup
opencode auth login
kilocode auth login
claude auth login
codex login
```

`GET /api/coders` reports `installed_unverified` or `unavailable`. Installation is not reported as authenticated; the first harmless execution verifies native CLI authentication.

## Run with Podman Compose

The checked-in Compose deployment builds a release Rust API image and a production nginx/Vue image. It uses host networking but binds both services to loopback only:

```bash
podman build -f Containerfile.backend -t localhost/lumen-ai-chat-api:dev .
podman build -f Containerfile.frontend -t localhost/lumen-ai-chat-web:dev .
podman compose --in-pod=false -f compose.yaml up -d --no-build
```

Open http://127.0.0.1:41789. Check service state and logs with:

```bash
podman compose --in-pod=false -f compose.yaml ps
podman logs lumen-ai-chat-api
podman logs lumen-ai-chat-web
```

The API container runs rootlessly with dropped capabilities, a read-only root filesystem, and a read-only project mount. Hermes configuration and its installed launcher tree are mounted read-only, with writable tmpfs overlays for cache, logs, and transient session files; its persistent child-session database remains intentionally read-only because Lumen invokes coders one request at a time. For other native coders, Compose mounts only explicit authentication/settings files as read-only seeds; the entrypoint copies those allowlisted files into a private tmpfs home. Package stores, histories, databases, and telemetry state are not mounted. Native CLIs can create transient runtime state without mutating host credentials; all container-side state disappears when the container is recreated. Coder source inspection still uses the temporary secret-filtered read-only snapshot described below. No project `.env` file is mounted or loaded by Compose.

Stop the deployment with:

```bash
podman compose --in-pod=false -f compose.yaml down
```

### Workspace safety

Coder processes run in one fixed workspace and cannot accept a working directory from an HTTP request:

```bash
export AI_CODER_WORKSPACE="/absolute/path/to/project"
```

The default is the gateway's startup directory. Child processes receive a scrubbed environment that excludes provider keys and application secrets while retaining standard OS paths needed to load native CLI credential files.

The gateway requires a loopback `APP_BIND`; every non-loopback bind is rejected. Remote access must be provided by a separately managed TLS and identity-aware reverse proxy rather than by exposing Lumen directly.

Coder execution is read-only by default. The gateway first creates a temporary, secret-filtered workspace snapshot (excluding `.git`, `.env*`, credentials, dependency caches, and build output), runs the coder inside that snapshot, and deletes it afterward. This prevents CLI session metadata or attempted edits from touching the source workspace.

- Hermes uses the `safe` toolset.
- OpenCode and KiloCode do not receive auto-approval flags.
- Claude Code uses `plan` permission mode.
- Codex uses the `read-only` sandbox.

Explicitly enable workspace edits only when intended:

```bash
export AI_CODER_ALLOW_WRITES=true
```

Even with writes enabled, the gateway prompt prohibits reading `.env`/credentials and prohibits commits, pushes, or history rewrites. Configure execution limits with `AI_CODER_TIMEOUT_SECONDS` (1–900, default 300) and `AI_CODER_MAX_CONCURRENCY` (1–16, default 2).

## Agent harness

`/api/chat` translates tools between provider dialects and hands any calls back to the caller. The harness runs them.

Turn it on in **Settings → Agent harness**, or call `POST /api/agent` directly. A request is answered by the same loop whichever backend serves it:

- **A hosted provider** (OpenAI, Anthropic, Gemini, or any OpenAI-compatible endpoint) is offered the tool registry. The harness executes each call, feeds the result back, and goes round again until the model answers or hits `AI_HARNESS_MAX_STEPS`.
- **A native coder CLI** already runs its own agent loop with its own tools. The harness delegates the turn to it rather than pretending to drive tools it does not control, and reports that it did so with a `delegated` frame.

### Tools

Every tool is confined to `AI_CODER_WORKSPACE`. Paths are normalised, checked against the same secret-file policy the CLI coders use, and re-checked after symlink resolution, so a path that reads clean but points outside the root is refused rather than followed.

| Tool | Purpose |
| --- | --- |
| `read_file` | Numbered lines, with `offset`/`limit` paging |
| `list_directory` | Entries, excluding anything a read would refuse |
| `search_text` | Literal, case-insensitive match across the workspace |
| `search_memory` | Word-overlap and symbol-name search over the codebase index |
| `write_file` | Create or replace a file (write mode only) |
| `edit_file` | Exact string replacement, refusing an ambiguous match (write mode only) |

`write_file` and `edit_file` appear only when `AI_CODER_ALLOW_WRITES=true`, and `search_memory` only when an index is available. A tool that could only fail is never advertised.

A tool error is returned to the model as a tool result, not raised as a failure: told that a path does not exist, a model lists the directory and tries again.

### Codebase index

`search_memory` is backed by [`harness-memory`](backend/harness-memory/README.md), a vector store with int8-quantised rows, an HNSW graph, and hybrid retrieval that fuses vector similarity with exact symbol-name matching through reciprocal rank fusion.

The vectors come from a hashed bag-of-tokens embedder, not a trained model, so retrieval ranks on **shared vocabulary rather than on meaning**: a query using the words the code uses finds it, and a paraphrase that shares none of them does not. The tool description tells the model exactly that, so it asks with identifiers and project terms instead of prose. The embedding width is pinned at 384 — MiniLM's — so substituting a real model later does not change the on-disk row format or require a reindex of anything but the vectors.

The indexer cuts each file into records a coding agent would want back — a function, a type, a Markdown heading — and falls back to fixed windows for files with no detectable definitions, so a README is retrievable rather than invisible. A successful `write_file` or `edit_file` re-indexes that file immediately, which is what stops the agent from reading its own stale work back.

```bash
export AI_HARNESS_MEMORY_DIR=/var/lib/lumen/memory   # must be writable
export AI_HARNESS_INDEX_ON_START=true
curl -X POST http://127.0.0.1:41788/api/harness/index
```

The default location is `$AI_CODER_WORKSPACE/data/memory`. Because the standard deployment mounts the workspace read-only, a failure to open the default location is logged and the harness runs without retrieval; naming the directory explicitly makes the same failure fatal at startup, since an operator who asked for retrieval should not get a service that silently lacks it.

### Transcript compaction

An assistant `tool_use` and the `tool_result` answering it are indivisible on every provider, and splitting them is a 400. The compactor therefore prunes whole turns — a user message and everything answering it — rather than individual messages, and the note recording what it dropped is a system message, so a later pass updates it instead of discarding the summary it just paid for.

## Direct provider APIs

Direct APIs remain available under the `api` coder namespace:

- `api/openai/<model>`
- `api/anthropic/<model>`
- `api/gemini/<model>`
- `api/ollama/<model>` when Ollama is explicitly enabled

Set any combination of hosted-provider credentials:

```bash
export OPENAI_API_KEY="..."
export OPENAI_MODEL="gpt-5-mini"

export ANTHROPIC_API_KEY="..."
export ANTHROPIC_MODEL="claude-sonnet-4-5"

export GEMINI_API_KEY="..."       # GOOGLE_API_KEY takes precedence
export GEMINI_MODEL="gemini-2.5-flash"

npm install
npm run dev
```

Provider-specific endpoint overrides:

```bash
export OPENAI_BASE_URL="https://api.openai.com/v1"
export ANTHROPIC_BASE_URL="https://api.anthropic.com"
export GEMINI_BASE_URL="https://generativelanguage.googleapis.com/v1beta"
```

Keys use each native protocol:

- OpenAI and compatible endpoints: bearer authorization
- Anthropic: `x-api-key` and `anthropic-version`
- Gemini: `x-goog-api-key`, never a query-string credential

The gateway never returns or intentionally logs credentials.

### Additional OpenAI-compatible endpoints

`AI_ENDPOINTS_JSON` accepts up to 20 endpoint definitions. Credentials remain in separate environment variables:

```bash
export GROQ_API_KEY="..."
export AI_ENDPOINTS_JSON='[
  {
    "id": "groq",
    "name": "Groq",
    "base_url": "https://api.groq.com/openai/v1",
    "model": "llama-3.3-70b-versatile",
    "api_key_env": "GROQ_API_KEY"
  }
]'
```

This appears as `api/groq/llama-3.3-70b-versatile`.

## Ollama is explicit opt-in only

Lumen never enables or selects Ollama because it happens to be installed. An Ollama-only direct-API deployment requires all selection and enablement flags:

```bash
ollama serve
ollama pull qwen3:4b
AI_DEFAULT_CODER=api AI_DEFAULT_PROVIDER=ollama \
  OLLAMA_ENABLED=true OLLAMA_MODEL=qwen3:4b npm run dev
```

Ollama is added only when `OLLAMA_ENABLED=true` or legacy `AI_PROVIDER=openai-compatible` is explicitly selected. `OLLAMA_MODEL`, `OLLAMA_BASE_URL`, and `OLLAMA_API_KEY` never enable it by themselves.

## Environment reference

| Variable | Default | Purpose |
| --- | --- | --- |
| `APP_BIND` | `127.0.0.1:41788` | Rust HTTP bind address |
| `AI_DEFAULT_CODER` | `hermes` | Default AI coder |
| `AI_DEFAULT_PROVIDER` | first provider for default coder | Default provider |
| `AI_CODER_ROUTES_JSON` | one default route per installed CLI | Coder/provider/model catalog |
| `AI_CODER_WORKSPACE` | startup directory | Fixed coder workspace |
| `AI_CODER_ALLOW_WRITES` | `false` | Explicit workspace-write opt-in |
| `AI_CODER_TIMEOUT_SECONDS` | `300` | Per-execution deadline |
| `AI_CODER_MAX_CONCURRENCY` | `2` | Concurrent coder process limit |
| `AI_CODER_HERMES_BIN` | discovered from `PATH` | Hermes executable override |
| `AI_CODER_OPENCODE_BIN` | discovered from `PATH` | OpenCode executable override |
| `AI_CODER_KILOCODE_BIN` | discovered from `PATH` | KiloCode executable override |
| `AI_CODER_CLAUDE_BIN` | discovered from `PATH` | Claude executable override |
| `AI_CODER_CODEX_BIN` | discovered from `PATH` | Codex executable override |
| `AI_HARNESS_MEMORY` | enabled | Set `off` to disable codebase retrieval entirely |
| `AI_HARNESS_MEMORY_DIR` | `$AI_CODER_WORKSPACE/data/memory` | Index location; naming it makes open failures fatal |
| `AI_HARNESS_INDEX_ON_START` | `false` | Build the index at startup |
| `AI_HARNESS_MAX_STEPS` | `12` | Model turns per agent run (1–50) |
| `AI_HARNESS_MEMORY_HITS` | `6` | Records retrieved before each run (0 disables pre-turn retrieval) |
| `AI_ENDPOINTS_JSON` | unset | Additional compatible APIs |
| `OLLAMA_ENABLED` | `false` | Explicit local inference opt-in |

Legacy `AI_PROVIDER`, `AI_BASE_URL`, `AI_MODEL`, and `AI_API_KEY` remain supported for explicitly configured direct-provider deployments.

## API

- `GET /api/health` — gateway status and backend counts
- `GET /api/ready` — readiness and selected default route
- `GET /api/metrics` — Prometheus text metrics for chat and agent requests, executed tool calls, indexed records, and coder capacity
- `GET /api/coders` — installed coder status, routes, workspace mode, and limits
- `GET /api/providers` — direct-provider metadata without credentials
- `GET /api/models` — combined coder and direct-provider model catalog
- `POST /api/chat` — selected CLI execution or provider-routed SSE stream
- `POST /api/agent` — agent harness run, streaming tool activity and the finished answer
- `GET /api/harness` — workspace, write mode, step limit, available tools, and index size
- `POST /api/harness/index` — rebuild the codebase index

Chat request shape:

```json
{
  "ai_coder": "hermes",
  "provider": "z.ai",
  "model": "hermes/z.ai/glm-5.2",
  "temperature": 0.7,
  "max_tokens": 4096,
  "messages": [
    { "role": "system", "content": "Be concise." },
    { "role": "user", "content": "Review this repository architecture." }
  ]
}
```

`POST /api/agent` takes the same body and streams JSON frames terminated by `[DONE]`. Each frame is tagged by `event`: `context`, `delegated`, `text`, `tool_call`, `tool_result`, `compacted`, and a final `outcome` carrying the answer, the step count, and whether the step limit was reached. A client that only understands text can read the `text` frames and ignore the rest.

```
data: {"event":"tool_call","step":1,"id":"call_1","name":"read_file","arguments":{"path":"README.md"}}
data: {"event":"tool_result","step":1,"id":"call_1","name":"read_file","output":"     1| # Lumen…","failed":false}
data: {"event":"text","step":2,"text":"Lumen routes chat through native coder CLIs."}
data: {"event":"outcome","text":"Lumen routes chat…","steps":2,"tool_calls":1,"stopped_at_step_limit":false}
data: [DONE]
```

Open http://127.0.0.1:41789. The Rust API binds to http://127.0.0.1:41788 and Vite proxies `/api` to it.

## Identity and stored conversations

Lumen serves one person by default: conversations live in that browser, and the
gateway neither knows nor asks who is asking. That is unchanged, and it is what
every existing install keeps doing.

A hosted deployment turns both on. Identity comes from a header set by a trusted
reverse proxy — the identity-aware proxy this README already requires for remote
access — rather than from credentials the gateway checks itself:

```bash
export AI_IDENTITY_MODE=proxy                  # default: single
export AI_IDENTITY_USER_HEADER=x-forwarded-user
export AI_STORE_PATH=/var/lib/lumen/conversations.db   # must be writable
```

`proxy` mode is only safe when nothing can reach the gateway except that proxy,
which is why the loopback bind guard stays. In `single` mode the header is
ignored entirely, so a deployment that was not told to trust a proxy cannot have
its user decided by whoever sends the request.

Conversations are then keyed by that identity. Every read and every write names
the user it is for: a conversation belonging to someone else reads as missing
rather than as forbidden, and an id cannot be taken over by saving onto it.

| Variable | Default | Purpose |
| --- | --- | --- |
| `AI_IDENTITY_MODE` | `single` | `proxy` to trust an identity header |
| `AI_IDENTITY_USER_HEADER` | `x-forwarded-user` | Header carrying the user id |
| `AI_IDENTITY_EMAIL_HEADER` | `x-forwarded-email` | Optional; empty disables |
| `AI_IDENTITY_NAME_HEADER` | `x-forwarded-preferred-username` | Optional; empty disables |
| `AI_STORE` | enabled | Set `off` to keep conversations in the browser |
| `AI_STORE_PATH` | `$AI_CODER_WORKSPACE/data/conversations.db` | Naming it makes open failures fatal |

`GET /api/me` reports who the gateway thinks is asking and whether it keeps
conversations; the client uses it to decide where to put them.

- `GET /api/conversations` — history, summaries only
- `GET /api/conversations/{id}` — one transcript
- `PUT /api/conversations/{id}` — save one, replacing what was there
- `DELETE /api/conversations/{id}` and `DELETE /api/conversations`

Note that the store is the deployment's first piece of persistent state: the API
container currently runs entirely on tmpfs with a read-only root, so a hosted
deployment needs a real volume for it.

## Serving more than one person

Three things have to be true together, and each is refused rather than assumed.

**Files.** The harness confines its tools to one root. With one person that was
the whole answer. With several it is not: a shared root anyone can write to is a
channel between them, and `search_memory` will retrieve whatever was left there.

```bash
export AI_WORKSPACE_MODE=per_user      # default: shared
```

`per_user` gives each person a subtree of their own, created on first use, and
an index of their own over it. `shared` stays correct for a read-only workspace —
one project, everyone reading the same files, nobody able to leave anything for
anyone. The combination that cannot be made safe, several people writing to one
root, is refused at startup.

**Spending.** An agent run is up to twelve model turns, each of which may call
tools whose output comes back as more input. Step count was the only ceiling,
and step count is not a budget.

```bash
export AI_QUOTA_REQUESTS_PER_WINDOW=200
export AI_QUOTA_TOKENS_PER_WINDOW=2000000
export AI_QUOTA_WINDOW_SECONDS=3600     # default: 3600
```

Both are per user and off by default. A refusal is `429` with `Retry-After`, and
`GET /api/me` reports what the caller has spent. Tokens are counted only where a
provider reports them — OpenAI-compatible providers are asked to include usage,
Anthropic reports input and output separately, Gemini repeats a running total —
and a run that reports nothing is charged nothing. A native coder CLI reports no
usage, so token budgets do not constrain those routes; request budgets do.

**The network.** Identity is asserted by a header, which is worth something only
if nothing but the proxy can set it. On loopback that is a property of the
machine; off it, it is a property of the network, which this process cannot
check.

```bash
export APP_BIND=0.0.0.0:41788
export AI_IDENTITY_MODE=proxy
export AI_TRUSTED_PROXY=true
export AI_IDENTITY_SHARED_SECRET="$(openssl rand -hex 32)"
```

All four, or the service refuses to start.

The secret is the part that is not merely an assertion. `AI_TRUSTED_PROXY` says
the network is arranged correctly and nothing can check that; a value only the
proxy knows, sent by it on every request and compared in constant time, is
something this process *can* check. Without it, any host that can reach the port
can claim to be any user, so off loopback it is required rather than encouraged.
On loopback it is optional, because there the machine is the boundary.

Relaxing the bind relaxes nothing else: a remote deployment with several users
still cannot share a writable workspace.

| Variable | Default | Purpose |
| --- | --- | --- |
| `AI_WORKSPACE_MODE` | `shared` | `per_user` for a subtree and index per person |
| `AI_QUOTA_REQUESTS_PER_WINDOW` | `0` (unlimited) | Requests one person may make per window |
| `AI_QUOTA_TOKENS_PER_WINDOW` | `0` (unlimited) | Tokens one person may spend per window |
| `AI_QUOTA_WINDOW_SECONDS` | `3600` | Window length, 60–86400 |
| `AI_TRUSTED_PROXY` | off | Assert that a proxy authenticates in front |
| `AI_IDENTITY_SHARED_SECRET` | unset | A value only the proxy knows; required off loopback |
| `AI_IDENTITY_SECRET_HEADER` | `x-lumen-proxy-secret` | Header the secret arrives in |

### Running the hosted deployment

```bash
podman compose -f compose.yaml -f compose.hosted.yaml up -d
```

The overlay adds the deployment's first piece of persistent state: a volume at
`/var/lib/lumen` holding the conversation store, the per-user workspaces, and the
per-user indexes. The base deployment runs entirely on tmpfs, which is right when
conversations live in a browser and the workspace is somebody's checkout; neither
is true here. Everything else about the hardening is unchanged — read-only root,
capabilities dropped, no privilege escalation — and the volume is the only thing
that changes about what the container can write.

Two consequences worth knowing before you run it:

The agent no longer sees the repository. In per-user mode `AI_CODER_WORKSPACE` is
the parent the subtrees hang off rather than a project, so it is the volume, not
the read-only project mount. Each user's agent sees that user's files.

The bind stays on loopback. Put an identity-aware proxy in front of
`127.0.0.1:41789` and have it set `X-Forwarded-User`; nothing else may be able to
reach that port, because the header is worth exactly what the network makes it
worth. Only move `APP_BIND` off loopback when something else is enforcing that,
and then `AI_TRUSTED_PROXY=true` is how you say so.

The first time a browser meets a deployment that keeps conversations, whatever it
already had is uploaded once and the local copy is left where it is. Turning the
store on therefore does not strand anybody's history, and turning it off again
does not lose it.

## Contributing

Work flows `feature/* → dev → staging → main`, by pull request at every step.
[CONTRIBUTING.md](CONTRIBUTING.md) has the rules and, importantly, which of them
GitHub can actually enforce here — this is a private repository on a Free
account, where branch protection is unavailable, so the promotion rules live in
a workflow and a local hook instead. Releases are tags on `main`:
[RELEASING.md](RELEASING.md).

Install the local push guard once per clone:

```bash
git config core.hooksPath githooks
```

## Quality gates

```bash
npm run check:server
npm run lint
npm run typecheck
npm test
npm run build
npm run test:e2e
npm run audit
npm run test:podman  # while the Compose deployment is running
```

`npm run check:server` covers the whole Cargo workspace — the gateway plus the `harness-memory` crate — as rustfmt, clippy at `-D warnings`, and every Rust test:

| Suite | Covers |
| --- | --- |
| `backend/src/**` unit tests | Configuration validation, route selection, provider protocol translation in both directions, generation bounds, CLI command construction and output normalization for all five coders, the read-only workspace copy, transcript compaction, path containment, tool behaviour, and the memory bridge |
| `backend/tests/api.rs` | The HTTP surface: health, readiness, Prometheus metrics, the provider and coder lists, the model catalog with its degraded-provider fallback, streamed chat, request refusals, the JSON 404, the body limit, and that the bare router carries no `/api` prefix |
| `backend/tests/harness.rs` | The agent loop end to end against a scripted upstream: a tool call becoming real work, a failed tool being reported rather than ending the run, the step limit, retrieval, delegation to a native coder, every provider protocol, and the `/agent`, `/harness`, `/harness/index` endpoints |
| `harness-memory` | Row payload round trips, crash-tail recovery, reopen, tombstones, HNSW recall against brute force, quantization, SIMD kernels, and the embedder |

`npm run lint` is the frontend's counterpart to clippy at `-D warnings`, which
the Rust side has had from the start. It is type-aware, so it catches what a type
checker will not: a promise nobody awaits, a `catch` that swallows something it
should not, a template binding a prop that does not exist. Adding it found six
floating promises in navigation and background loads, each of which would have
surfaced as an unhandled rejection.

`npm test` runs the Vitest unit suite over the frontend: the chat store (SSE and harness frame parsing, streaming, abort, regeneration, persistence), the settings store, and each component — `AppSidebar`, `ChatComposer`, `MessageBubble` — in isolation.

`npm run test:e2e` starts a full stack (mock provider, the real gateway with mock coder executables, Vite) and runs three suites against it in order, stopping at the first failure:

| Suite | Runner | Covers |
| --- | --- | --- |
| `tests/contract/config-guards.test.mjs` | `node --test` | That naming a local model does not by itself enable local inference, and that `OLLAMA_ENABLED` does |
| `tests/contract/provider-contract.test.mjs` | `node --test` | The service surface, the model catalog, a streamed answer from each of the five CLI adapters and all four hosted protocols, route conflicts, and the harness driving its own loop over every protocol and delegating to a native coder |
| `tests/e2e/*.spec.ts` | Playwright | The browser, below |
| `tests/e2e/accessibility.spec.ts` | Playwright + axe | WCAG 2 A and AA across both themes, every settings section, agent output, and keyboard reachability |
| `tests/hosted/*.spec.ts` | Playwright | The server-backed deployment, against a second stack with a store and proxy identity: history surviving a browser that remembers nothing, following the person rather than the machine, and never crossing between two people |

`npm run test:podman` runs `tests/podman/podman-smoke.spec.ts` against a Compose deployment you already have running — production images, hardened containers, and a real Hermes invocation rather than a mock. It is the only suite that is not mocked, which is why it is a separate config with its own timeouts.

The Playwright specs live in `tests/e2e/` and cover both pages in full:

| Spec | Covers |
| --- | --- |
| `chat-view.spec.ts` | The empty state and its suggestions, sending, streaming, titling, the thinking indicator, stopping, share, the overflow menu, keyboard shortcuts, deep links, and reload |
| `chat-composer.spec.ts` | Model grouping and selection, what the request carries, an unavailable catalog, degraded providers, attachment acceptance and refusal, and the growing textarea |
| `message-bubble.spec.ts` | Copy, feedback, regeneration, failures, and every agent-mode surface: retrieval counts, tool activity, failed and pending tools, and the step-limit notice |
| `app-sidebar.spec.ts` | History ordering, search, opening, deleting, new chats, and the profile row |
| `settings-general.spec.ts` | Navigation, the profile form, preferences and generation settings — including that they reach the wire — appearance and system theme, notifications, and data controls |
| `settings-harness.spec.ts` | The coder list and the harness report, both asserted against the live gateway, plus reindexing, an index that is unavailable, and agent mode changing which endpoint the chat calls |
| `routing.spec.ts` | Every route, the unknown-path redirect, and browser history |
| `mobile.spec.ts` | The phone layout: the sidebar overlay, its backdrop, and sending from a phone |

Chromium is resolved from the system rather than downloaded (`CHROMIUM_PATH`, defaulting to `/usr/bin/chromium`), so `npm ci` runs with `PLAYWRIGHT_SKIP_BROWSER_DOWNLOAD=1`. To run the browser suite against a stack you already have up, use `BASE_URL=http://127.0.0.1:41789 npm run test:e2e:only`; `npm run test:e2e -- --grep <pattern>` passes arguments through to Playwright.
