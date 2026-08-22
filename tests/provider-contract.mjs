import assert from 'node:assert/strict'

const baseURL = process.env.API_URL ?? 'http://127.0.0.1:41788'
const modelsResponse = await fetch(`${baseURL}/api/models`)
assert.equal(modelsResponse.ok, true)
const { models, default_ai_coder: defaultAiCoder, default_provider: defaultProvider } = await modelsResponse.json()
assert.equal(defaultAiCoder, 'hermes')
assert.equal(defaultProvider, 'openai')
assert.deepEqual(new Set(models.map(model => model.key)), new Set([
  'hermes/openai/openai-test',
  'hermes/anthropic/anthropic-test',
  'hermes/gemini/gemini-test',
  'ollama/ollama/ollama-test',
  'hermes/z.ai/glm-5.2',
  'codex/codex/gpt-5.6',
]))

async function chat(aiCoder, provider, model, includeRouteFields = true) {
  const response = await fetch(`${baseURL}/api/chat`, {
    method: 'POST',
    headers: { 'content-type': 'application/json', accept: 'text/event-stream' },
    body: JSON.stringify({
      ...(includeRouteFields ? { ai_coder: aiCoder, provider } : {}),
      model: `${aiCoder}/${provider}/${model}`,
      temperature: 0.3,
      max_tokens: 256,
      messages: [{ role: 'system', content: 'Be concise.' }, { role: 'user', content: `Hello from ${provider}` }],
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
  assert.match(content, /Mock response received:/)
}

await chat('hermes', 'openai', 'openai-test')
await chat('hermes', 'anthropic', 'anthropic-test')
await chat('hermes', 'gemini', 'gemini-test')
await chat('hermes', 'z.ai', 'glm-5.2', false)
await chat('codex', 'codex', 'gpt-5.6')
await chat('ollama', 'ollama', 'ollama-test')
const conflict = await fetch(`${baseURL}/api/chat`, {
  method: 'POST',
  headers: { 'content-type': 'application/json' },
  body: JSON.stringify({
    ai_coder: 'codex',
    provider: 'codex',
    model: 'hermes/openai/openai-test',
    messages: [{ role: 'user', content: 'This route must be rejected.' }],
  }),
})
assert.equal(conflict.status, 400)
assert.equal((await conflict.json()).error.type, 'conflicting_route')
console.log('Provider contracts passed: Hermes multi-provider, Codex fixed-provider, and explicit Ollama opt-in')
