import assert from 'node:assert/strict'

const baseURL = process.env.API_URL ?? 'http://127.0.0.1:41788'
const readyResponse = await fetch(`${baseURL}/api/ready`)
assert.equal(readyResponse.ok, true)
assert.equal((await readyResponse.json()).status, 'ready')
const metricsResponse = await fetch(`${baseURL}/api/metrics`)
assert.equal(metricsResponse.ok, true)
assert.match(await metricsResponse.text(), /lumen_chat_requests_total/)
const corsResponse = await fetch(`${baseURL}/api/health`, { headers: { origin: 'https://attacker.invalid' } })
assert.equal(corsResponse.headers.has('access-control-allow-origin'), false)
const modelsResponse = await fetch(`${baseURL}/api/models`)
assert.equal(modelsResponse.ok, true)
const { models, default_ai_coder: defaultAiCoder, default_provider: defaultProvider } = await modelsResponse.json()
assert.equal(defaultAiCoder, 'hermes')
assert.equal(defaultProvider, 'z.ai')
assert.deepEqual(new Set(models.map(model => model.key)), new Set([
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

const codersResponse = await fetch(`${baseURL}/api/coders`)
assert.equal(codersResponse.ok, true)
const coderPayload = await codersResponse.json()
assert.equal(coderPayload.allow_writes, false)
assert.equal(coderPayload.max_concurrency, 2)
assert.deepEqual(new Set(coderPayload.coders.map(coder => coder.id)), new Set(['hermes', 'opencode', 'kilocode', 'claude', 'codex']))
assert.ok(coderPayload.coders.every(coder => coder.status === 'installed_unverified' && coder.route_count === 1))

async function chat(aiCoder, provider, model, expected, includeRouteFields = true) {
  const response = await fetch(`${baseURL}/api/chat`, {
    method: 'POST',
    headers: { 'content-type': 'application/json', accept: 'text/event-stream' },
    body: JSON.stringify({
      ...(includeRouteFields ? { ai_coder: aiCoder, provider } : {}),
      model: `${aiCoder}/${provider}/${model}`,
      temperature: 0.3,
      max_tokens: 256,
      messages: [{ role: 'system', content: 'Be concise.' }, { role: 'user', content: `Hello from ${aiCoder}` }],
    }),
  })
  assert.equal(response.ok, true)
  let content = ''
  for (const block of (await response.text()).split(/\r?\n\r?\n/)) {
    const line = block.split(/\r?\n/).find(value => value.startsWith('data:'))
    if (!line || line.slice(5).trim() === '[DONE]') continue
    const event = JSON.parse(line.slice(5).trim())
    if (event.error) throw new Error(event.error.message)
    content += event.choices?.[0]?.delta?.content ?? ''
  }
  assert.match(content, expected)
}

await chat('hermes', 'z.ai', 'glm-5.2', /Mock coder response: hermes read-only/, false)
await chat('opencode', 'openrouter', 'qwen/test', /Mock coder response: opencode\/kilocode read-only/)
await chat('kilocode', 'kilocode', 'kilo-test', /Mock coder response: opencode\/kilocode read-only/)
await chat('claude', 'claude', 'sonnet-test', /Mock coder response: claude read-only/)
await chat('codex', 'codex', 'gpt-test', /Mock coder response: codex read-only/)
await chat('api', 'openai', 'openai-test', /Mock response received:/)
await chat('api', 'anthropic', 'anthropic-test', /Mock response received:/)
await chat('api', 'gemini', 'gemini-test', /Mock response received:/)
await chat('api', 'ollama', 'ollama-test', /Mock response received:/)

const conflict = await fetch(`${baseURL}/api/chat`, {
  method: 'POST',
  headers: { 'content-type': 'application/json' },
  body: JSON.stringify({
    ai_coder: 'codex',
    provider: 'codex',
    model: 'hermes/z.ai/glm-5.2',
    messages: [{ role: 'user', content: 'This route must be rejected.' }],
  }),
})
assert.equal(conflict.status, 400)
assert.equal((await conflict.json()).error.type, 'conflicting_route')
// ---------------------------------------------------------------------------
// The agent harness
// ---------------------------------------------------------------------------

const harnessResponse = await fetch(`${baseURL}/api/harness`)
assert.equal(harnessResponse.ok, true)
const harness = await harnessResponse.json()
assert.equal(harness.writes_enabled, false)
assert.ok(harness.max_steps >= 1)
// A read-only workspace must not advertise tools that can only fail.
assert.deepEqual(new Set(harness.tools), new Set(['read_file', 'list_directory', 'search_text', 'search_memory']))
assert.equal(harness.memory.records, 0, 'a fresh index holds nothing')

// Indexing is what makes search_memory answer anything at all.
const indexResponse = await fetch(`${baseURL}/api/harness/index`, { method: 'POST' })
assert.equal(indexResponse.ok, true)
const indexed = await indexResponse.json()
assert.ok(indexed.files > 0, `Expected the workspace to yield files, received ${indexed.files}`)
assert.ok(indexed.records > 0, `Expected the workspace to yield records, received ${indexed.records}`)
assert.equal((await (await fetch(`${baseURL}/api/harness`)).json()).memory.records, indexed.records)

/// Read one agent run, returning every frame it emitted.
async function agent(body) {
  const response = await fetch(`${baseURL}/api/agent`, {
    method: 'POST',
    headers: { 'content-type': 'application/json', accept: 'text/event-stream' },
    body: JSON.stringify(body),
  })
  assert.equal(response.ok, true, `Agent request failed with ${response.status}`)
  const frames = []
  for (const block of (await response.text()).split(/\r?\n\r?\n/)) {
    const line = block.split(/\r?\n/).find(value => value.startsWith('data:'))
    if (!line) continue
    const data = line.slice(5).trim()
    if (data === '[DONE]') { frames.push({ event: 'done' }); continue }
    frames.push(JSON.parse(data))
  }
  return frames
}

// A hosted provider: the harness drives the loop and terminates it.
for (const [aiCoder, provider, model] of [
  ['api', 'openai', 'openai-test'],
  ['api', 'anthropic', 'anthropic-test'],
  ['api', 'gemini', 'gemini-test'],
  ['api', 'ollama', 'ollama-test'],
]) {
  const frames = await agent({
    ai_coder: aiCoder,
    provider,
    model: `${aiCoder}/${provider}/${model}`,
    messages: [{ role: 'user', content: 'Summarise the repository.' }],
  })
  assert.deepEqual(frames.at(-1), { event: 'done' }, `${provider} stream must terminate`)
  const outcome = frames.find(frame => frame.event === 'outcome')
  assert.ok(outcome, `${provider} produced no outcome: ${JSON.stringify(frames)}`)
  assert.match(outcome.text, /Mock response received:/, `${provider} lost the model's answer`)
  assert.equal(outcome.stopped_at_step_limit, false, `${provider} did not finish`)
}

// A native CLI coder: already an agent, so the harness delegates one pass.
const delegated = await agent({
  ai_coder: 'hermes',
  provider: 'z.ai',
  model: 'hermes/z.ai/glm-5.2',
  messages: [{ role: 'user', content: 'Summarise the repository.' }],
})
assert.ok(delegated.some(frame => frame.event === 'delegated'), `Expected delegation: ${JSON.stringify(delegated)}`)
const delegatedOutcome = delegated.find(frame => frame.event === 'outcome')
assert.match(delegatedOutcome.text, /Mock coder response: hermes read-only/)
assert.equal(delegatedOutcome.tool_calls, 0, 'a delegated run runs no tools of its own')

const finalMetrics = await (await fetch(`${baseURL}/api/metrics`)).text()
const requestCount = Number(finalMetrics.match(/lumen_chat_requests_total (\d+)/)?.[1] ?? 0)
assert.ok(requestCount >= 10, `Expected chat request metric to mutate, received ${requestCount}`)
console.log('Contracts passed: five native AI coder CLIs, four direct provider protocols, and the agent harness over both')
