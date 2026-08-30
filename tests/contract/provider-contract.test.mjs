/// The contract the gateway owes a client, asserted against a running instance.
///
/// `tests/run-e2e.mjs` starts that instance with mock coder executables and a
/// mock upstream, and points `API_URL` at it. Every case here is a claim the
/// frontend or an operator depends on: which routes exist, what a stream looks
/// like from each of them, and what the harness does with both kinds of route.

import assert from 'node:assert/strict'
import { describe, it } from 'node:test'

const baseURL = process.env.API_URL ?? 'http://127.0.0.1:41788'

const nativeCoders = [
  { coder: 'hermes', provider: 'z.ai', model: 'glm-5.2', expected: /Mock coder response: hermes read-only/, routeFields: false },
  { coder: 'opencode', provider: 'openrouter', model: 'qwen/test', expected: /Mock coder response: opencode\/kilocode read-only/ },
  { coder: 'kilocode', provider: 'kilocode', model: 'kilo-test', expected: /Mock coder response: opencode\/kilocode read-only/ },
  { coder: 'claude', provider: 'claude', model: 'sonnet-test', expected: /Mock coder response: claude read-only/ },
  { coder: 'codex', provider: 'codex', model: 'gpt-test', expected: /Mock coder response: codex read-only/ },
]

const hostedProviders = [
  { coder: 'api', provider: 'openai', model: 'openai-test' },
  { coder: 'api', provider: 'anthropic', model: 'anthropic-test' },
  { coder: 'api', provider: 'gemini', model: 'gemini-test' },
  { coder: 'api', provider: 'ollama', model: 'ollama-test' },
]

const json = async path => {
  const response = await fetch(`${baseURL}${path}`)
  assert.equal(response.ok, true, `GET ${path} answered ${response.status}`)
  return response.json()
}

/// Every `data:` payload in an SSE body, with `[DONE]` normalised to a frame so
/// a truncated stream fails on the assertion rather than on a missing element.
const frames = body => body
  .split(/\r?\n\r?\n/)
  .map(block => block.split(/\r?\n/).find(line => line.startsWith('data:')))
  .filter(Boolean)
  .map(line => line.slice(5).trim())
  .map(data => (data === '[DONE]' ? { event: 'done' } : JSON.parse(data)))

async function chat({ coder, provider, model, routeFields = true }) {
  const response = await fetch(`${baseURL}/api/chat`, {
    method: 'POST',
    headers: { 'content-type': 'application/json', accept: 'text/event-stream' },
    body: JSON.stringify({
      ...(routeFields ? { ai_coder: coder, provider } : {}),
      model: `${coder}/${provider}/${model}`,
      temperature: 0.3,
      max_tokens: 256,
      messages: [{ role: 'system', content: 'Be concise.' }, { role: 'user', content: `Hello from ${coder}` }],
    }),
  })
  assert.equal(response.ok, true, `chat answered ${response.status}`)

  let content = ''
  for (const frame of frames(await response.text())) {
    if (frame.event === 'done') continue
    if (frame.error) throw new Error(frame.error.message)
    content += frame.choices?.[0]?.delta?.content ?? ''
  }
  return content
}

async function agent(body) {
  const response = await fetch(`${baseURL}/api/agent`, {
    method: 'POST',
    headers: { 'content-type': 'application/json', accept: 'text/event-stream' },
    body: JSON.stringify(body),
  })
  assert.equal(response.ok, true, `the agent endpoint answered ${response.status}`)
  return frames(await response.text())
}

describe('the service surface', () => {
  it('reports itself ready', async () => {
    assert.equal((await json('/api/ready')).status, 'ready')
  })

  it('publishes metrics', async () => {
    const response = await fetch(`${baseURL}/api/metrics`)
    assert.equal(response.ok, true)
    assert.match(await response.text(), /lumen_chat_requests_total/)
  })

  it('grants no cross-origin access to a site that asks for it', async () => {
    const response = await fetch(`${baseURL}/api/health`, { headers: { origin: 'https://attacker.invalid' } })
    assert.equal(response.headers.has('access-control-allow-origin'), false)
  })
})

describe('the model catalog', () => {
  it('offers every configured route under its canonical key', async () => {
    const payload = await json('/api/models')
    assert.equal(payload.default_ai_coder, 'hermes')
    assert.equal(payload.default_provider, 'z.ai')
    assert.deepEqual(new Set(payload.models.map(model => model.key)), new Set([
      'hermes/z.ai/glm-5.2',
      'opencode/openrouter/qwen/test',
      'kilocode/kilocode/kilo-test',
      'claude/claude/sonnet-test',
      'codex/codex/gpt-test',
      'api/openai/openai-test',
      'api/anthropic/anthropic-test',
      'api/gemini/gemini-test',
      'api/ollama/ollama-test',
    ]))
  })

  it('reports every coder as installed with one route, and the workspace as read-only', async () => {
    const payload = await json('/api/coders')
    assert.equal(payload.allow_writes, false)
    assert.equal(payload.max_concurrency, 2)
    assert.deepEqual(
      new Set(payload.coders.map(coder => coder.id)),
      new Set(['hermes', 'opencode', 'kilocode', 'claude', 'codex']),
    )
    assert.ok(payload.coders.every(coder => coder.status === 'installed_unverified' && coder.route_count === 1))
  })
})

describe('native AI coder CLIs', () => {
  for (const route of nativeCoders) {
    it(`${route.coder} is invoked read-only and its answer is streamed back`, async () => {
      assert.match(await chat(route), route.expected)
    })
  }
})

describe('hosted provider protocols', () => {
  for (const route of hostedProviders) {
    it(`${route.provider} is translated in both directions`, async () => {
      assert.match(await chat(route), /Mock response received:/)
    })
  }
})

describe('route selection', () => {
  it('refuses a request whose fields and model key disagree', async () => {
    const response = await fetch(`${baseURL}/api/chat`, {
      method: 'POST',
      headers: { 'content-type': 'application/json' },
      body: JSON.stringify({
        ai_coder: 'codex',
        provider: 'codex',
        model: 'hermes/z.ai/glm-5.2',
        messages: [{ role: 'user', content: 'This route must be rejected.' }],
      }),
    })
    assert.equal(response.status, 400)
    assert.equal((await response.json()).error.type, 'conflicting_route')
  })
})

describe('the agent harness', () => {
  it('advertises only the tools a read-only workspace can serve', async () => {
    const harness = await json('/api/harness')
    assert.equal(harness.writes_enabled, false)
    assert.ok(harness.max_steps >= 1)
    assert.deepEqual(
      new Set(harness.tools),
      new Set(['read_file', 'list_directory', 'search_text', 'search_memory']),
    )
    assert.equal(harness.memory.records, 0, 'a fresh index holds nothing')
  })

  it('indexes the workspace, which is what gives search_memory anything to find', async () => {
    const response = await fetch(`${baseURL}/api/harness/index`, { method: 'POST' })
    assert.equal(response.ok, true)
    const indexed = await response.json()
    assert.ok(indexed.files > 0, `expected files, received ${indexed.files}`)
    assert.ok(indexed.records > 0, `expected records, received ${indexed.records}`)

    const harness = await json('/api/harness')
    assert.equal(harness.memory.records, indexed.records, 'the status must report what was just indexed')
  })

  for (const route of hostedProviders) {
    it(`drives and terminates its own loop over ${route.provider}`, async () => {
      const emitted = await agent({
        ai_coder: route.coder,
        provider: route.provider,
        model: `${route.coder}/${route.provider}/${route.model}`,
        messages: [{ role: 'user', content: 'Summarise the repository.' }],
      })

      assert.deepEqual(emitted.at(-1), { event: 'done' }, 'the stream must terminate')
      const outcome = emitted.find(frame => frame.event === 'outcome')
      assert.ok(outcome, `no outcome was emitted: ${JSON.stringify(emitted)}`)
      assert.match(outcome.text, /Mock response received:/, "the model's answer was lost")
      assert.equal(outcome.stopped_at_step_limit, false)
    })
  }

  it('delegates one pass to a native coder rather than driving tools it does not own', async () => {
    const emitted = await agent({
      ai_coder: 'hermes',
      provider: 'z.ai',
      model: 'hermes/z.ai/glm-5.2',
      messages: [{ role: 'user', content: 'Summarise the repository.' }],
    })

    assert.ok(
      emitted.some(frame => frame.event === 'delegated'),
      `expected delegation: ${JSON.stringify(emitted)}`,
    )
    const outcome = emitted.find(frame => frame.event === 'outcome')
    assert.match(outcome.text, /Mock coder response: hermes read-only/)
    assert.equal(outcome.tool_calls, 0, 'a delegated run runs no tools of its own')
  })
})

describe('metrics', () => {
  it('counted every chat this suite sent', async () => {
    const body = await (await fetch(`${baseURL}/api/metrics`)).text()
    const count = Number(body.match(/lumen_chat_requests_total (\d+)/)?.[1] ?? 0)
    assert.ok(count >= nativeCoders.length + hostedProviders.length, `chat counter did not move: ${count}`)
  })
})
