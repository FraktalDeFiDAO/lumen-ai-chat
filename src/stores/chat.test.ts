import { beforeEach, describe, expect, it, vi } from 'vitest'
import { createPinia, setActivePinia } from 'pinia'
import { applyHarnessFrame, parseHarnessBlock, parseSSEBlock, readSSE, useChatStore } from './chat'
import { useSettingsStore } from './settings'
import type { Message } from '@/types/chat'

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
    store.models = [{ id: 'model-a', key: 'hermes/provider-a/model-a', name: 'Model A', provider: 'provider-a', provider_name: 'Provider A', ai_coder: 'hermes', ai_coder_name: 'Hermes' }]
    store.selectedModel = 'hermes/provider-a/model-a'
    await store.sendMessage('Hi')
    expect(store.activeConversation?.messages.map(message => message.content)).toEqual(['Hi', 'Hello there'])
    const [, options] = fetchMock.mock.calls[0]!
    expect(fetchMock.mock.calls[0]![0]).toBe('/api/chat')
    expect(JSON.parse(options.body)).toEqual({ ai_coder: 'hermes', provider: 'provider-a', model: 'hermes/provider-a/model-a', temperature: 0.7, max_tokens: 4096, messages: [{ role: 'system', content: 'Respond in English. Use a balanced level of detail.' }, { role: 'user', content: 'Hi' }] })
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

describe('harness stream', () => {
  it('parses frames, the terminator, and errors', () => {
    expect(parseHarnessBlock('data: {"event":"text","text":"Hi"}')).toEqual({ done: false, frame: { event: 'text', text: 'Hi' } })
    expect(parseHarnessBlock('data: [DONE]')).toEqual({ done: true })
    expect(parseHarnessBlock('data: not-json')).toEqual({ done: false })
    expect(parseHarnessBlock('data: {"error":{"message":"Harness failed"}}')).toEqual({ done: false, error: 'Harness failed' })
  })

  it('folds a whole run into one message', () => {
    const message = { id: 'm', role: 'assistant', content: '', createdAt: 0 } as Message
    applyHarnessFrame(message, { event: 'context', records: 3 })
    applyHarnessFrame(message, { event: 'tool_call', id: 'call_1', name: 'read_file', arguments: { path: 'a.rs' } })
    applyHarnessFrame(message, { event: 'tool_result', id: 'call_1', output: 'contents', failed: false })
    applyHarnessFrame(message, { event: 'text', text: 'It reads a.rs.' })
    applyHarnessFrame(message, { event: 'outcome', text: 'It reads a.rs.', stopped_at_step_limit: false })

    expect(message.contextRecords).toBe(3)
    expect(message.content).toBe('It reads a.rs.')
    expect(message.tools).toEqual([{ id: 'call_1', name: 'read_file', arguments: { path: 'a.rs' }, output: 'contents', failed: false }])
    expect(message.incomplete).toBeUndefined()
  })

  it('marks a run that hit the step limit and ignores unknown frames', () => {
    const message = { id: 'm', role: 'assistant', content: 'partial', createdAt: 0 } as Message
    applyHarnessFrame(message, { event: 'delegated' })
    applyHarnessFrame(message, { event: 'a_frame_from_a_newer_server' })
    applyHarnessFrame(message, { event: 'outcome', text: 'ignored', stopped_at_step_limit: true })
    expect(message.content).toBe('partial')
    expect(message.incomplete).toBe(true)
  })

  it('joins text from several steps the way the harness does', () => {
    const message = { id: 'm', role: 'assistant', content: '', createdAt: 0 } as Message
    applyHarnessFrame(message, { event: 'text', text: 'First.' })
    applyHarnessFrame(message, { event: 'text', text: 'Second.' })
    expect(message.content).toBe('First.\n\nSecond.')
  })
})

describe('agent mode', () => {
  beforeEach(() => {
    localStorage.clear(); setActivePinia(createPinia()); id = 0
    vi.stubGlobal('crypto', { randomUUID: () => `id-${++id}` })
    vi.restoreAllMocks()
  })

  it('routes to the harness and records the tools it ran', async () => {
    const fetchMock = vi.fn().mockResolvedValue(sseResponse([
      'data: {"event":"tool_call","step":1,"id":"call_1","name":"read_file","arguments":{"path":"notes.md"}}\n\n',
      'data: {"event":"tool_result","step":1,"id":"call_1","name":"read_file","output":"forty-two","failed":false}\n\n',
      'data: {"event":"text","step":2,"text":"The note says forty-two."}\n\n',
      'data: {"event":"outcome","text":"The note says forty-two.","steps":2,"tool_calls":1,"stopped_at_step_limit":false}\n\ndata: [DONE]\n\n',
    ]))
    vi.stubGlobal('fetch', fetchMock)
    useSettingsStore().settings.agentMode = true

    const store = useChatStore()
    await store.sendMessage('what is in notes.md?')

    expect(fetchMock.mock.calls[0]![0]).toBe('/api/agent')
    const answer = store.activeConversation?.messages[1]
    expect(answer?.content).toBe('The note says forty-two.')
    expect(answer?.tools).toEqual([{ id: 'call_1', name: 'read_file', arguments: { path: 'notes.md' }, output: 'forty-two', failed: false }])
    expect(answer?.error).toBeUndefined()
  })

  /// A run whose only product is the work it did is still a successful run.
  it('keeps a tool-only answer rather than calling it an empty response', async () => {
    vi.stubGlobal('fetch', vi.fn().mockResolvedValue(sseResponse([
      'data: {"event":"tool_call","step":1,"id":"call_1","name":"write_file","arguments":{"path":"a.rs"}}\n\n',
      'data: {"event":"tool_result","step":1,"id":"call_1","name":"write_file","output":"Wrote 2 lines to a.rs.","failed":false}\n\n',
      'data: {"event":"outcome","text":"","steps":2,"tool_calls":1,"stopped_at_step_limit":true}\n\ndata: [DONE]\n\n',
    ])))
    useSettingsStore().settings.agentMode = true

    const store = useChatStore()
    await store.sendMessage('write a.rs')
    const answer = store.activeConversation?.messages[1]
    expect(answer?.error).toBeUndefined()
    expect(answer?.tools).toHaveLength(1)
    expect(answer?.incomplete).toBe(true)
  })

  it('surfaces a harness error on the message', async () => {
    vi.stubGlobal('fetch', vi.fn().mockResolvedValue(sseResponse([
      'data: {"error":{"type":"upstream_error","message":"The selected AI provider is unavailable"}}\n\ndata: [DONE]\n\n',
    ])))
    useSettingsStore().settings.agentMode = true

    const store = useChatStore()
    await store.sendMessage('do something')
    expect(store.activeConversation?.messages[1]?.error).toBe('The selected AI provider is unavailable')
  })
})
