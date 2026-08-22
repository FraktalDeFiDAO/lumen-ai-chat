# Lumen AI Chat

Lumen is a full-stack AI chat application with a Vue 3 interface and a Rust/Axum model gateway. It supports native OpenAI, Anthropic, and Gemini APIs plus Ollama and other OpenAI-compatible endpoints.

## Stack

- Vue 3, TypeScript, Vue Router, Pinia, Tailwind CSS 4, Vite
- Rust, Axum, Tokio, Reqwest
- Provider-normalized streaming over server-sent events
- Vitest unit tests and headless Chromium end-to-end tests

## Run with Ollama

Start Ollama and install a model:

```bash
ollama serve
ollama pull qwen3:4b
```

Then run the Rust API and Vue development server:

```bash
npm install
AI_PROVIDER=openai-compatible AI_MODEL=qwen3:4b npm run dev
```

Open http://127.0.0.1:41789. The Rust API binds to http://127.0.0.1:41788 and Vite proxies `/api` to it.

## Providers

Provider configuration belongs in the shell that starts Lumen. Do not commit credentials.

OpenAI:

```bash
export AI_PROVIDER=openai
export OPENAI_API_KEY="..."
export AI_MODEL="gpt-5-mini"
npm run dev
```

Anthropic:

```bash
export AI_PROVIDER=anthropic
export ANTHROPIC_API_KEY="..."
export AI_MODEL="claude-sonnet-4-5"
npm run dev
```

Gemini Developer API:

```bash
export AI_PROVIDER=gemini
export GEMINI_API_KEY="..."
export AI_MODEL="gemini-2.5-flash"
npm run dev
```

Other OpenAI-compatible services:

```bash
export AI_PROVIDER=openai-compatible
export AI_BASE_URL="https://provider.example/v1"
export AI_API_KEY="..." # omit when the endpoint requires no authentication
export AI_MODEL="provider-model-id"
npm run dev
```

### Server settings

| Variable | Default | Purpose |
| --- | --- | --- |
| `APP_BIND` | `127.0.0.1:41788` | Rust HTTP bind address |
| `AI_PROVIDER` | `openai-compatible` | `openai`, `anthropic`, `gemini`, or `openai-compatible` |
| `AI_BASE_URL` | provider-specific | Optional endpoint override |
| `AI_MODEL` | `llama3.2` | Default model ID |
| `AI_API_KEY` | unset | Generic endpoint credential override |
| `OPENAI_API_KEY` | unset | Native OpenAI credential |
| `ANTHROPIC_API_KEY` | unset | Native Anthropic credential |
| `GOOGLE_API_KEY` / `GEMINI_API_KEY` | unset | Native Gemini credential |

The server never logs credentials. Gemini keys are sent in `x-goog-api-key`, Anthropic keys in `x-api-key`, and OpenAI/OpenAI-compatible credentials as bearer tokens.

## Quality gates

```bash
npm run typecheck
npm test
npm run build
npm run check:server
npm run test:e2e
```

The E2E command starts an isolated mock provider, Rust gateway, and Vite server before exercising the desktop and mobile flows in Chromium.

## Features

- Streaming responses with stop, errors, and regenerate
- Provider-backed model discovery and selection
- Persistent conversation history and search
- Text/code file attachments with validation
- Copy, feedback, sharing, deletion, and data controls
- Responsive desktop/mobile layout
- Local profile and response preferences

Conversation data and preferences stay in browser localStorage. Model requests pass through the local Rust gateway; the gateway does not persist prompts or credentials.
