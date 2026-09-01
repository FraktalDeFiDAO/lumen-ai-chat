# Context

The vocabulary this codebase uses. These terms are used precisely and
consistently in the code; this file is where they are defined.

## Routing

**Route** — the canonical triple `<coder>/<provider>/<model>`, for example
`hermes/z.ai/glm-5.2`. It is what a client sends and what the gateway resolves.
Its three parts are never referred to separately as an identifier.

**Coder** — a native AI coding CLI that Lumen executes as a program: Hermes,
OpenCode, KiloCode, Claude Code, Codex. A coder is *already* an agent, which is
why the harness delegates to one rather than driving it.

**Provider** — a hosted model API reached over HTTP: OpenAI, Anthropic, Gemini,
Ollama. Distinguished from a coder by who runs the loop.

**Execution** — which of the two a route resolves to, `Native` or a provider.

## The agent

**Harness** — the loop that turns a model's tool calls into work. It is
provider-neutral: it speaks in `ToolDef` and `ToolCall`, and the gateway decides
what those look like on the wire.

**Tool** — something the harness can run on the model's behalf, confined to a
workspace. `read_file`, `list_directory`, `search_text`, `search_memory`, and
with writes enabled `write_file` and `edit_file`.

**Step** — one model turn inside the harness loop. A step may call several tools.
The loop is bounded by `max_steps`.

**Transcript** — the message list as it stands, including tool results, in a form
that can be handed back in to continue.

## Tenancy

**Workspace** — a canonical root plus the write permission that applies inside
it. Every path from a model is resolved into one, and re-checked after symlinks.

**Tenancy** — the workspace and index a given user is served. `Shared` gives
everyone the same one; `PerUser` gives each person a subtree.

**Identity** — who is asking, resolved from the request. `local` in a single-user
deployment; whatever the proxy asserted in a hosted one.

## Retrieval

**Index** — the codebase memory: records extracted from source files, embedded,
and searchable. One per workspace.

**Record** — one retrieval unit: a function, a type, a heading, or a window of
prose. Not a file and not a line.

**Fresh / stale** — whether the index still describes the tree. It goes stale
when the workspace changes outside the harness, which the harness cannot see.

## Conversations

**Conversation** — a titled transcript belonging to one user. Kept in the browser
in a single-user deployment, and on the server in a hosted one.

**Promotion** — moving a commit that has already passed CI from one long-lived
branch to the next. See CONTRIBUTING.md.
