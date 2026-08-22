import assert from 'node:assert/strict'

const baseURL = process.env.API_URL ?? 'http://127.0.0.1:41788'
const modelsResponse = await fetch(`${baseURL}/api/models`)
assert.equal(modelsResponse.ok, true)
const { models } = await modelsResponse.json()
assert.deepEqual(new Set(models.map(model => model.provider)), new Set(['openai', 'anthropic', 'gemini', 'ollama']))

async function chat(provider, model) {
  const response = await fetch(`${baseURL}/api/chat`, {
    method: 'POST',
    headers: { 'content-type': 'application/json', accept: 'text/event-stream' },
    body: JSON.stringify({
      provider,
      model,
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

await chat('openai', 'openai-test')
await chat('anthropic', 'anthropic-test')
await chat('gemini', 'gemini-test')
await chat('ollama', 'ollama-test')
console.log('Provider contracts passed: OpenAI, Anthropic, Gemini, and OpenAI-compatible')
