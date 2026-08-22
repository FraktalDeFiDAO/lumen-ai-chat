# Lumen AI Chat

Lumen is a Vue 3 + Rust/Axum chat application that invokes real AI coding agents and also supports direct hosted-model APIs. Selections use the canonical route `<aiCoder>/<provider>/<model>`, for example `hermes/z.ai/glm-5.2` or `codex/codex/gpt-5.6`.

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

### Workspace safety

Coder processes run in one fixed workspace and cannot accept a working directory from an HTTP request:

```bash
export AI_CODER_WORKSPACE="/absolute/path/to/project"
```

The default is the gateway's startup directory. Child processes receive a scrubbed environment that excludes provider keys and application secrets while retaining standard OS paths needed to load native CLI credential files.

Native coder routes require a loopback `APP_BIND` by default. A non-loopback bind is rejected unless the operator explicitly accepts that deployment risk with `AI_CODER_ALLOW_REMOTE=true` and places the gateway behind appropriate authentication.

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
| `AI_CODER_ALLOW_REMOTE` | `false` | Explicit non-loopback coder-route opt-in |
| `AI_CODER_TIMEOUT_SECONDS` | `300` | Per-execution deadline |
| `AI_CODER_MAX_CONCURRENCY` | `2` | Concurrent coder process limit |
| `AI_CODER_HERMES_BIN` | discovered from `PATH` | Hermes executable override |
| `AI_CODER_OPENCODE_BIN` | discovered from `PATH` | OpenCode executable override |
| `AI_CODER_KILOCODE_BIN` | discovered from `PATH` | KiloCode executable override |
| `AI_CODER_CLAUDE_BIN` | discovered from `PATH` | Claude executable override |
| `AI_CODER_CODEX_BIN` | discovered from `PATH` | Codex executable override |
| `AI_ENDPOINTS_JSON` | unset | Additional compatible APIs |
| `OLLAMA_ENABLED` | `false` | Explicit local inference opt-in |

Legacy `AI_PROVIDER`, `AI_BASE_URL`, `AI_MODEL`, and `AI_API_KEY` remain supported for explicitly configured direct-provider deployments.

## API

- `GET /api/health` — gateway status and backend counts
- `GET /api/coders` — installed coder status, routes, workspace mode, and limits
- `GET /api/providers` — direct-provider metadata without credentials
- `GET /api/models` — combined coder and direct-provider model catalog
- `POST /api/chat` — selected CLI execution or provider-routed SSE stream

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

Open http://127.0.0.1:41789. The Rust API binds to http://127.0.0.1:41788 and Vite proxies `/api` to it.

## Quality gates

```bash
npm run check:server
npm run typecheck
npm test
npm run build
npm run test:e2e
```

The E2E gate invokes isolated executable mocks through all five CLI adapters, validates read-only command construction and output normalization, exercises all native direct-provider protocols, and verifies the complete desktop/mobile UI in Chromium.
