import { beforeEach, describe, expect, it, vi } from 'vitest'
import { createPinia, setActivePinia } from 'pinia'
import { parseSSEBlock, readSSE, useChatStore } from './chat'

let id = 0
function sseResponse(parts: string[]) {
  const encoder = new TextEncoder()
  return new Response(new ReadableStream({ start(controller) { parts.forEach(part => controller.enqueue(encoder.encode(part))); controller.close() } }), { status: 200, headers: { 'Content-Type': 'text/event-stream' } })
}

describe('SSE parser', () => {
  it('parses content, done events, and ignores malformed data', () => {
    expect(parseSSEBlock('data: {"choices":[{"delta":{"content":"Hi"}}]}')).toEqual({ done: false, content: 'Hi' })
    expect(parseSSEBlock('data: [DONE]')).toEqual({ done: true, content: '' })
    expect(parseSSEBlock('data: not-json')).toEqual({ done: false, content: '' })
    expect(parseSSEBlock('data: {"error":{"message":"Provider failed"}}')).toEqual({ done: false, content: '', error: 'Provider failed' })
  })
  it('reads fragmented SSE chunks', async () => {
    const chunks: string[] = []
    await readSSE(sseResponse(['data: {"choices":[{"delta":{"con', 'tent":"Hel"}}]}\n\ndata: {"choices":[{"delta":{"content":"lo"}}]}\n\n', 'data: [DONE]\n\n']), value => chunks.push(value))
    expect(chunks.join('')).toBe('Hello')
  })
  it('surfaces normalized provider stream errors', async () => {
    await expect(readSSE(sseResponse(['data: {"error":{"message":"Provider failed"}}\n\ndata: [DONE]\n\n']), () => undefined)).rejects.toThrow('Provider failed')
  })
})

describe('chat store', () => {
  beforeEach(() => {
    localStorage.clear(); setActivePinia(createPinia()); id = 0
    vi.stubGlobal('crypto', { randomUUID: () => `id-${++id}` })
    vi.restoreAllMocks()
  })
  it('creates and removes conversations', () => {
    const store = useChatStore(); const chat = store.createConversation('A useful title')
    expect(store.activeId).toBe(chat.id); expect(chat.title).toBe('A useful title')
    store.deleteConversation(chat.id); expect(store.conversations).toHaveLength(0)
  })
  it('ignores empty messages', async () => {
    const store = useChatStore(); await store.sendMessage('   '); expect(store.conversations).toHaveLength(0)
  })
  it('streams a real API response and sends the selected model', async () => {
    const fetchMock = vi.fn().mockResolvedValue(sseResponse(['data: {"choices":[{"delta":{"content":"Hello"}}]}\n\n', 'data: {"choices":[{"delta":{"content":" there"}}]}\n\ndata: [DONE]\n\n']))
    vi.stubGlobal('fetch', fetchMock)
    const store = useChatStore()
    store.models = [{ id: 'model-a', key: 'provider-a::model-a', name: 'Model A', provider: 'provider-a', provider_name: 'Provider A' }]
    store.selectedModel = 'provider-a::model-a'
    await store.sendMessage('Hi')
    expect(store.activeConversation?.messages.map(message => message.content)).toEqual(['Hi', 'Hello there'])
    const [, options] = fetchMock.mock.calls[0]!
    expect(fetchMock.mock.calls[0]![0]).toBe('/api/chat')
    expect(JSON.parse(options.body)).toEqual({ provider: 'provider-a', model: 'model-a', temperature: 0.7, max_tokens: 4096, messages: [{ role: 'system', content: 'Respond in English. Use a balanced level of detail.' }, { role: 'user', content: 'Hi' }] })
  })
  it('aborts safely without leaving an empty assistant response', async () => {
    vi.stubGlobal('fetch', vi.fn((_url: string, options: RequestInit) => new Promise((_resolve, reject) => {
      options.signal?.addEventListener('abort', () => reject(new DOMException('Aborted', 'AbortError')))
    })))
    const store = useChatStore(); const pending = store.sendMessage('Please wait')
    expect(store.isGenerating).toBe(true)
    store.stopGenerating(); await pending
    expect(store.isGenerating).toBe(false)
    expect(store.activeConversation?.messages).toHaveLength(1)
    expect(store.activeConversation?.messages[0]?.role).toBe('user')
  })
  it('stores request failures on the assistant message and can regenerate', async () => {
    const fetchMock = vi.fn()
      .mockResolvedValueOnce(new Response(JSON.stringify({ error: 'Provider unavailable' }), { status: 503, headers: { 'Content-Type': 'application/json' } }))
      .mockResolvedValueOnce(sseResponse(['data: {"choices":[{"delta":{"content":"Recovered"}}]}\n\ndata: [DONE]\n\n']))
    vi.stubGlobal('fetch', fetchMock)
    const store = useChatStore(); await store.sendMessage('Try this')
    const failed = store.activeConversation?.messages[1]
    expect(failed?.error).toBe('Provider unavailable')
    await store.regenerate(failed?.id)
    expect(store.activeConversation?.messages[1]?.content).toBe('Recovered')
    expect(store.activeConversation?.messages[1]?.error).toBeUndefined()
  })
  it('extracts nested server error messages', async () => {
    vi.stubGlobal('fetch', vi.fn().mockResolvedValue(new Response(JSON.stringify({ error: { message: 'Invalid provider request' } }), { status: 400, headers: { 'Content-Type': 'application/json' } })))
    const store = useChatStore(); await store.sendMessage('Try this')
    expect(store.activeConversation?.messages[1]?.error).toBe('Invalid provider request')
  })
})
