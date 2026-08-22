import { createServer } from 'node:http'

const port = Number(process.env.MOCK_AI_PORT ?? 41800)
const server = createServer((request, response) => {
  if (request.method === 'GET' && request.url === '/v1/models') {
    response.writeHead(200, { 'content-type': 'application/json' })
    response.end(JSON.stringify({ object: 'list', data: [{ id: 'lumen-test', object: 'model', owned_by: 'local' }] }))
    return
  }

  if (request.method === 'POST' && request.url === '/v1/chat/completions') {
    let body = ''
    request.setEncoding('utf8')
    request.on('data', chunk => { body += chunk })
    request.on('end', () => {
      const payload = JSON.parse(body)
      const lastMessage = payload.messages?.at(-1)?.content ?? ''
      response.writeHead(200, {
        'content-type': 'text/event-stream',
        'cache-control': 'no-cache',
        connection: 'keep-alive',
      })
      const text = `Mock response received: ${lastMessage.slice(0, 80)}`
      for (const token of text.match(/.{1,12}/g) ?? []) {
        response.write(`data: ${JSON.stringify({ choices: [{ delta: { content: token } }] })}\n\n`)
      }
      response.end('data: [DONE]\n\n')
    })
    return
  }

  response.writeHead(404, { 'content-type': 'application/json' })
  response.end(JSON.stringify({ error: 'not found' }))
})

server.listen(port, '127.0.0.1', () => console.log(`mock-provider-ready:${port}`))
for (const signal of ['SIGTERM', 'SIGINT']) process.on(signal, () => server.close(() => process.exit(0)))
