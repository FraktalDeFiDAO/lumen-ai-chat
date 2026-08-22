# Lumen AI Chat

Lumen is a full-stack, multi-provider AI chat application with a Vue 3 interface and a Rust/Axum gateway. A single running server can discover and use OpenAI, Anthropic, Google Gemini, Ollama, and any number of OpenAI-compatible endpoints simultaneously.

## Capabilities

- Native OpenAI Chat Completions streaming
- Native Anthropic Messages streaming
- Native Google Gemini `streamGenerateContent`
- Ollama, vLLM, LM Studio, Groq, Together, and other OpenAI-compatible services
- Concurrent model discovery across every configured provider
- Provider-grouped model selection in the composer
- Per-request provider routing, temperature, maximum output tokens, and system instructions
- Normalized SSE responses, cancellation, errors, and regeneration
- Persistent conversations, profile, generation preferences, and themes
- Validated text/code attachments, sharing, copying, and feedback

## Run locally with Ollama

```bash
ollama serve
ollama pull qwen3:4b
npm install
OLLAMA_MODEL=qwen3:4b npm run dev
```

Open http://127.0.0.1:41789. The Rust API binds to http://127.0.0.1:41788 and Vite proxies `/api` to it.

## Configure multiple native providers

Set any combination of credentials before starting Lumen. Every configured provider appears in the same model selector.

```bash
export OPENAI_API_KEY="..."
export OPENAI_MODEL="gpt-5-mini"

export ANTHROPIC_API_KEY="..."
export ANTHROPIC_MODEL="claude-sonnet-4-5"

export GEMINI_API_KEY="..."       # GOOGLE_API_KEY takes precedence when present
export GEMINI_MODEL="gemini-2.5-flash"

export OLLAMA_MODEL="qwen3:4b"
export AI_DEFAULT_PROVIDER="ollama"

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

- OpenAI and compatible endpoints: `Authorization: Bearer ...`
- Anthropic: `x-api-key` and `anthropic-version`
- Gemini: `x-goog-api-key` (never placed in the URL)

The gateway never returns or intentionally logs credentials.

## Add arbitrary OpenAI-compatible endpoints

`AI_ENDPOINTS_JSON` accepts an array of endpoint definitions. Credentials remain in separate environment variables; configuration references only the variable name.

```bash
export GROQ_API_KEY="..."
export TOGETHER_API_KEY="..."
export AI_ENDPOINTS_JSON='[
  {
    "id": "groq",
    "name": "Groq",
    "base_url": "https://api.groq.com/openai/v1",
    "model": "llama-3.3-70b-versatile",
    "api_key_env": "GROQ_API_KEY"
  },
  {
    "id": "together",
    "name": "Together AI",
    "base_url": "https://api.together.xyz/v1",
    "model": "meta-llama/Llama-3.3-70B-Instruct-Turbo",
    "api_key_env": "TOGETHER_API_KEY"
  }
]'
```

Up to 20 custom endpoints can be configured. Provider IDs must contain only letters, numbers, `-`, or `_`.

## Environment reference

| Variable | Default | Purpose |
| --- | --- | --- |
| `APP_BIND` | `127.0.0.1:41788` | Rust HTTP bind address |
| `AI_DEFAULT_PROVIDER` | `ollama` | Provider used when a request omits one |
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
| `AI_ENDPOINTS_JSON` | unset | Additional OpenAI-compatible endpoints |

Legacy `AI_PROVIDER`, `AI_BASE_URL`, `AI_MODEL`, and `AI_API_KEY` remain supported for single-provider deployments.

## API

- `GET /api/health` — gateway status and configured provider count
- `GET /api/providers` — configured provider metadata without credentials
- `GET /api/models` — concurrently aggregated, normalized model catalog
- `POST /api/chat` — provider-routed normalized SSE stream

Chat request shape:

```json
{
  "provider": "anthropic",
  "model": "claude-sonnet-4-5",
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

The E2E gate starts isolated OpenAI, Anthropic, Gemini, and OpenAI-compatible mock surfaces, validates every provider protocol through the Rust gateway, then exercises the complete desktop/mobile application in headless Chromium.
