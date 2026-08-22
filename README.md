# Lumen AI Chat

Lumen is a full-stack, multi-coder and multi-provider AI chat application with a Vue 3 interface and a Rust/Axum gateway. Models use the canonical route `<aiCoder>/<provider>/<model>`, such as `hermes/z.ai/glm-5.2` or `codex/codex/gpt-5.6`.

## Capabilities

- Native OpenAI Chat Completions streaming
- Native Anthropic Messages streaming
- Native Google Gemini `streamGenerateContent`
- Ollama, vLLM, LM Studio, Groq, Together, and other OpenAI-compatible services
- Concurrent model discovery across every configured provider
- AI-coder and provider-grouped model selection in the composer
- Per-request AI-coder/provider routing, temperature, maximum output tokens, and system instructions
- Normalized SSE responses, cancellation, errors, and regeneration
- Persistent conversations, profile, generation preferences, and themes
- Validated text/code attachments, sharing, copying, and feedback

## Run locally with Ollama

```bash
ollama serve
ollama pull qwen3:4b
npm install
AI_DEFAULT_CODER=ollama AI_DEFAULT_PROVIDER=ollama \
  OLLAMA_ENABLED=true OLLAMA_MODEL=qwen3:4b npm run dev
```

Open http://127.0.0.1:41789. The Rust API binds to http://127.0.0.1:41788 and Vite proxies `/api` to it.

## Configure multiple native providers

Set any combination of hosted-provider credentials before starting Lumen. Native hosted providers are routes under the default `hermes` AI coder, and every configured route appears in the same model selector. Local inference is never enabled implicitly.

```bash
export OPENAI_API_KEY="..."
export OPENAI_MODEL="gpt-5-mini"

export ANTHROPIC_API_KEY="..."
export ANTHROPIC_MODEL="claude-sonnet-4-5"

export GEMINI_API_KEY="..."       # GOOGLE_API_KEY takes precedence when present
export GEMINI_MODEL="gemini-2.5-flash"

npm run dev
```

Optional provider-specific endpoint overrides:

```bash
export OPENAI_BASE_URL="https://api.openai.com/v1"
export ANTHROPIC_BASE_URL="https://api.anthropic.com"
export GEMINI_BASE_URL="https://generativelanguage.googleapis.com/v1beta"
export OLLAMA_BASE_URL="http://127.0.0.1:11434/v1"
```

Keys are sent using each native protocol:

- OpenAI and compatible endpoints: `Authorization: Bearer ***`
- Anthropic: `x-api-key` and `anthropic-version`
- Gemini: `x-goog-api-key` (never placed in the URL)

The gateway never returns or intentionally logs credentials.

## Add arbitrary OpenAI-compatible endpoints

`AI_ENDPOINTS_JSON` accepts an array of endpoint definitions. Credentials remain in separate environment variables; configuration references only the variable name. Set `ai_coder` and `provider` for a multi-provider coder. If `ai_coder` is omitted, the endpoint is treated as a fixed-provider coder and its `id` is used for both path segments.

```bash
export ZAI_API_KEY="..."
export CODEX_API_KEY="..."
export AI_ENDPOINTS_JSON='[
  {
    "id": "hermes-zai",
    "name": "Z.AI",
    "ai_coder": "hermes",
    "ai_coder_name": "Hermes",
    "provider": "z.ai",
    "base_url": "https://api.z.ai/api/paas/v4",
    "model": "glm-5.2",
    "api_key_env": "ZAI_API_KEY"
  },
  {
    "id": "codex",
    "name": "Codex",
    "base_url": "https://example-codex-compatible.invalid/v1",
    "model": "gpt-5.6",
    "api_key_env": "CODEX_API_KEY"
  }
]'
```

Those entries appear as `hermes/z.ai/glm-5.2` and `codex/codex/gpt-5.6`. The second route follows the fixed-provider rule: coder and provider are both `codex`.

Up to 20 custom endpoints can be configured. Coder/provider IDs may contain letters, numbers, `.`, `-`, or `_`.

## Environment reference

| Variable | Default | Purpose |
| --- | --- | --- |
| `APP_BIND` | `127.0.0.1:41788` | Rust HTTP bind address |
| `AI_DEFAULT_CODER` | `hermes` | AI coder used when a request omits one |
| `AI_DEFAULT_PROVIDER` | first provider for the default coder | Provider used when a request omits one |
| `OPENAI_API_KEY` | unset | Enables native OpenAI |
| `OPENAI_BASE_URL` | OpenAI production API | Optional OpenAI endpoint override |
| `OPENAI_MODEL` | `gpt-5-mini` | OpenAI fallback model |
| `ANTHROPIC_API_KEY` | unset | Enables native Anthropic |
| `ANTHROPIC_BASE_URL` | Anthropic production API | Optional Anthropic endpoint override |
| `ANTHROPIC_MODEL` | `claude-sonnet-4-5` | Anthropic fallback model |
| `GOOGLE_API_KEY` / `GEMINI_API_KEY` | unset | Enables native Gemini |
| `GEMINI_BASE_URL` | Gemini Developer API | Optional Gemini endpoint override |
| `GEMINI_MODEL` | `gemini-2.5-flash` | Gemini fallback model |
| `OLLAMA_BASE_URL` | `http://127.0.0.1:11434/v1` | Local/compatible endpoint |
| `OLLAMA_MODEL` | `llama3.2` | Ollama fallback model |
| `OLLAMA_API_KEY` | unset | Optional credential for remote Ollama-compatible servers |
| `OLLAMA_ENABLED` | `false` | Explicitly enables local Ollama; never inferred from installation |
| `AI_ENDPOINTS_JSON` | unset | Additional OpenAI-compatible endpoints |

Ollama is added only when `OLLAMA_ENABLED=true`, an `OLLAMA_*` endpoint/model variable is explicitly set, or the legacy configuration explicitly selects `AI_PROVIDER=openai-compatible`. Because the default coder remains `hermes`, an Ollama-only deployment must also explicitly set `AI_DEFAULT_CODER=ollama`. If no hosted or explicitly enabled custom/local provider exists, the gateway exits with a configuration error instead of silently choosing local inference.

Legacy `AI_PROVIDER`, `AI_BASE_URL`, `AI_MODEL`, and `AI_API_KEY` remain supported for explicitly configured single-provider deployments.

## API

- `GET /api/health` — gateway status and configured provider count
- `GET /api/providers` — configured provider metadata without credentials
- `GET /api/models` — concurrently aggregated, normalized model catalog
- `POST /api/chat` — provider-routed normalized SSE stream

Chat request shape:

```json
{
  "ai_coder": "hermes",
  "provider": "anthropic",
  "model": "hermes/anthropic/claude-sonnet-4-5",
  "temperature": 0.7,
  "max_tokens": 4096,
  "messages": [
    { "role": "system", "content": "Be concise." },
    { "role": "user", "content": "Hello" }
  ]
}
```

## Quality gates

```bash
npm run check:server
npm run typecheck
npm test
npm run build
npm run test:e2e
```

The E2E gate starts isolated OpenAI, Anthropic, Gemini, Z.AI-compatible, Codex-compatible, and explicitly opted-in Ollama mock surfaces. It validates Hermes multi-provider paths, a fixed-provider coder path, every upstream protocol, and the complete desktop/mobile application in headless Chromium.
