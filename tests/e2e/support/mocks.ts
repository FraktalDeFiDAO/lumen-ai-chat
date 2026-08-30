import type { Page, Request } from '@playwright/test'

const SSE_HEADERS = { 'content-type': 'text/event-stream', 'cache-control': 'no-cache' }

/// The chat endpoint speaks OpenAI's chunk shape; the store only reads
/// `choices[0].delta.content`, so these frames are exactly what it parses.
export function chatStream(text: string, chunkSize = 8): string {
  const chunks = text.match(new RegExp(`.{1,${chunkSize}}`, 'gs')) ?? []
  return [
    ...chunks.map(chunk => `data: ${JSON.stringify({ choices: [{ delta: { content: chunk } }] })}\n\n`),
    'data: [DONE]\n\n',
  ].join('')
}

export interface HarnessFrame {
  event: 'context' | 'text' | 'tool_call' | 'tool_result' | 'outcome'
  step?: number
  id?: string
  name?: string
  arguments?: unknown
  output?: string
  failed?: boolean
  text?: string
  records?: number
  stopped_at_step_limit?: boolean
}

export function harnessStream(frames: HarnessFrame[]): string {
  return [...frames.map(frame => `data: ${JSON.stringify(frame)}\n\n`), 'data: [DONE]\n\n'].join('')
}

/// Serve a scripted `/api/chat` stream and hand back every request body the page
/// sent, so a spec can assert what the settings actually put on the wire.
export function mockChat(page: Page, text = 'Mocked assistant reply.'): { requests: Array<Record<string, unknown>> } {
  const requests: Array<Record<string, unknown>> = []
  void page.route('**/api/chat', async route => {
    requests.push(readJson(route.request()))
    await route.fulfill({ status: 200, headers: SSE_HEADERS, body: chatStream(text) })
  })
  return { requests }
}

export function mockAgent(page: Page, frames: HarnessFrame[]): { requests: Array<Record<string, unknown>> } {
  const requests: Array<Record<string, unknown>> = []
  void page.route('**/api/agent', async route => {
    requests.push(readJson(route.request()))
    await route.fulfill({ status: 200, headers: SSE_HEADERS, body: harnessStream(frames) })
  })
  return { requests }
}

/// A stream that never arrives, so the page stays in its generating state for as
/// long as the spec needs to look at it.
export function mockHangingChat(page: Page, holdMs = 15_000) {
  void page.route('**/api/chat', async route => {
    await new Promise(resolve => setTimeout(resolve, holdMs))
    await route.fulfill({ status: 200, headers: SSE_HEADERS, body: chatStream('Too late.') }).catch(() => {})
  })
}

export function mockJson(page: Page, pattern: string, payload: unknown, status = 200) {
  return page.route(pattern, route => route.fulfill({
    status,
    contentType: 'application/json',
    body: JSON.stringify(payload),
  }))
}

export function mockFailure(page: Page, pattern: string, status: number, message: string) {
  return page.route(pattern, route => route.fulfill({
    status,
    contentType: 'application/json',
    body: JSON.stringify({ error: { message } }),
  }))
}

function readJson(request: Request): Record<string, unknown> {
  try { return JSON.parse(request.postData() ?? '{}') as Record<string, unknown> }
  catch { return {} }
}

export const MODEL_KEY_DEFAULT = 'hermes/z.ai/glm-5.2'
