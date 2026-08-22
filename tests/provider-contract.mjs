import assert from 'node:assert/strict'

const baseURL = process.env.API_URL ?? 'http://127.0.0.1:41788'
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
assert.equal(coderPayload.allow_remote, false)
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
console.log('Contracts passed: five native AI coder CLIs plus four direct provider protocols')
