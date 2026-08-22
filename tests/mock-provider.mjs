import { createServer } from 'node:http'

const port = Number(process.env.MOCK_AI_PORT ?? 41800)
function json(response, payload) {
  response.writeHead(200, { 'content-type': 'application/json' })
  response.end(JSON.stringify(payload))
}
function sse(response, events) {
  response.writeHead(200, { 'content-type': 'text/event-stream', 'cache-control': 'no-cache', connection: 'keep-alive' })
  for (const event of events) response.write(event)
  response.end()
}
function chunks(text) { return text.match(/.{1,12}/g) ?? [] }

const server = createServer((request, response) => {
  if (request.method === 'GET' && request.url === '/v1/models') {
    const anthropic = Boolean(request.headers['x-api-key'])
    const id = anthropic ? 'anthropic-test' : 'openai-test'
    json(response, { data: [{ id, display_name: anthropic ? 'Anthropic Test' : 'OpenAI Test' }] })
    return
  }
  if (request.method === 'GET' && request.url === '/v1beta/models') {
    json(response, { models: [{ name: 'models/gemini-test', displayName: 'Gemini Test' }] })
    return
  }
  if (request.method === 'GET' && request.url === '/ollama/v1/models') {
    json(response, { data: [{ id: 'ollama-test', name: 'ollama-test' }] })
    return
  }
  if (request.method === 'GET' && request.url === '/zai/v1/models') {
    json(response, { data: [{ id: 'glm-5.2', name: 'GLM 5.2' }] })
    return
  }
  if (request.method === 'GET' && request.url === '/codex/v1/models') {
    json(response, { data: [{ id: 'gpt-5.6', name: 'GPT 5.6' }] })
    return
  }

  if (request.method === 'POST') {
    let body = ''
    request.setEncoding('utf8')
    request.on('data', chunk => { body += chunk })
    request.on('end', () => {
      const payload = JSON.parse(body)
      if (request.url === '/v1/messages') {
        const last = payload.messages?.at(-1)?.content ?? ''
        const events = chunks(`Mock response received: ${last.slice(0, 80)}`).map(token => `event: content_block_delta\ndata: ${JSON.stringify({ type: 'content_block_delta', delta: { type: 'text_delta', text: token } })}\n\n`)
        events.push('event: message_stop\ndata: {"type":"message_stop"}\n\n')
        sse(response, events)
        return
      }
      if (request.url?.startsWith('/v1beta/models/gemini-test:streamGenerateContent')) {
        const last = payload.contents?.at(-1)?.parts?.[0]?.text ?? ''
        sse(response, [
          `data: ${JSON.stringify({ candidates: [{ content: { parts: [{ text: `Mock response received: ${last.slice(0, 80)}` }] } }] })}\n\n`,
          `data: ${JSON.stringify({ candidates: [{ finishReason: 'STOP' }] })}\n\n`,
        ])
        return
      }
      if (request.url === '/v1/chat/completions' || request.url === '/ollama/v1/chat/completions' || request.url === '/zai/v1/chat/completions' || request.url === '/codex/v1/chat/completions') {
        const last = payload.messages?.at(-1)?.content ?? ''
        const events = chunks(`Mock response received: ${last.slice(0, 80)}`).map(token => `data: ${JSON.stringify({ choices: [{ delta: { content: token } }] })}\n\n`)
        events.push('data: [DONE]\n\n')
        sse(response, events)
        return
      }
      response.writeHead(404).end()
    })
    return
  }

  response.writeHead(404, { 'content-type': 'application/json' })
  response.end(JSON.stringify({ error: 'not found' }))
})

server.listen(port, '127.0.0.1', () => console.log(`mock-provider-ready:${port}`))
for (const signal of ['SIGTERM', 'SIGINT']) process.on(signal, () => server.close(() => process.exit(0)))
