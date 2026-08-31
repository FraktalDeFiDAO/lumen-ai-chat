import { beforeEach, describe, expect, it, vi } from 'vitest'
import { LocalBackend, ServerBackend, StoreUnavailable, detectBackend, markMigrated, migrateLocalHistory } from './persistence'
import type { Conversation } from '@/types/chat'

const STORAGE_KEY = 'lumen-conversations-v1'

function conversation(id: string, title: string, updatedAt = 10): Conversation {
  return {
    id,
    title,
    createdAt: updatedAt,
    updatedAt,
    messages: [
      { id: 'm1', role: 'user', content: 'a question', createdAt: updatedAt, feedback: null },
      { id: 'm2', role: 'assistant', content: 'an answer', createdAt: updatedAt, feedback: null },
    ],
  }
}

function jsonResponse(status: number, body: unknown) {
  return new Response(body === undefined ? null : JSON.stringify(body), {
    status,
    headers: { 'Content-Type': 'application/json' },
  })
}

describe('LocalBackend', () => {
  beforeEach(() => localStorage.clear())

  it('reads back what it saved', async () => {
    const backend = new LocalBackend()
    await backend.save(conversation('c1', 'First'))

    expect((await backend.list()).map(item => item.id)).toEqual(['c1'])
    expect((await backend.load('c1'))?.title).toBe('First')
    expect(backend.listIncludesMessages).toBe(true)
  })

  it('replaces a conversation in place rather than duplicating it', async () => {
    const backend = new LocalBackend()
    await backend.save(conversation('c1', 'First'))
    await backend.save(conversation('c1', 'Renamed', 20))

    const all = await backend.list()
    expect(all).toHaveLength(1)
    expect(all[0].title).toBe('Renamed')
  })

  it('removes one and clears all', async () => {
    const backend = new LocalBackend()
    await backend.save(conversation('c1', 'One'))
    await backend.save(conversation('c2', 'Two'))

    await backend.remove('c1')
    expect((await backend.list()).map(item => item.id)).toEqual(['c2'])

    await backend.clear()
    expect(await backend.list()).toEqual([])
  })

  it('treats unreadable storage as an empty history', async () => {
    localStorage.setItem(STORAGE_KEY, 'not json')
    expect(await new LocalBackend().list()).toEqual([])
  })

  it('answers with nothing for a conversation it does not hold', async () => {
    expect(await new LocalBackend().load('missing')).toBeNull()
  })
})

describe('ServerBackend', () => {
  beforeEach(() => vi.restoreAllMocks())

  it('turns a list of summaries into conversations with no transcript yet', async () => {
    vi.stubGlobal('fetch', vi.fn().mockResolvedValue(jsonResponse(200, {
      conversations: [
        { id: 'c1', title: 'First', created_at: 1, updated_at: 2, message_count: 4 },
      ],
    })))

    const listed = await new ServerBackend().list()
    expect(listed).toEqual([{ id: 'c1', title: 'First', messages: [], createdAt: 1, updatedAt: 2 }])
    expect(new ServerBackend().listIncludesMessages).toBe(false)
  })

  it('reads a transcript back into the shape the UI holds', async () => {
    vi.stubGlobal('fetch', vi.fn().mockResolvedValue(jsonResponse(200, {
      id: 'c1',
      title: 'With tools',
      created_at: 1,
      updated_at: 2,
      messages: [
        {
          id: 'm1',
          role: 'assistant',
          content: 'an answer',
          created_at: 3,
          feedback: 'up',
          extra: {
            tools: [{ id: 'call-1', name: 'read_file', output: 'ok' }],
            contextRecords: 4,
            incomplete: true,
          },
        },
      ],
    })))

    const loaded = await new ServerBackend().load('c1')
    expect(loaded?.createdAt).toBe(1)
    expect(loaded?.messages[0]).toMatchObject({
      id: 'm1',
      role: 'assistant',
      createdAt: 3,
      feedback: 'up',
      contextRecords: 4,
      incomplete: true,
    })
    expect(loaded?.messages[0]!.tools).toHaveLength(1)
  })

  it('sends the transcript in the shape the server keeps', async () => {
    const fetchMock = vi.fn().mockResolvedValue(jsonResponse(200, {}))
    vi.stubGlobal('fetch', fetchMock)

    const saved = conversation('c1', 'First')
    saved.messages[1].tools = [{ id: 'call-1', name: 'read_file', output: 'ok' }]
    saved.messages[1].contextRecords = 2
    await new ServerBackend().save(saved)

    const [url, options] = fetchMock.mock.calls[0]
    expect(url).toBe('/api/conversations/c1')
    expect(options.method).toBe('PUT')
    const body = JSON.parse(options.body)
    expect(body).toMatchObject({ id: 'c1', title: 'First', created_at: 10, updated_at: 10 })
    expect(body.messages[0]).toMatchObject({ id: 'm1', role: 'user', created_at: 10 })
    expect(body.messages[1].extra).toEqual({
      tools: [{ id: 'call-1', name: 'read_file', output: 'ok' }],
      contextRecords: 2,
    })
  })

  it('stores nothing extra for a plain message', async () => {
    const fetchMock = vi.fn().mockResolvedValue(jsonResponse(200, {}))
    vi.stubGlobal('fetch', fetchMock)

    await new ServerBackend().save(conversation('c1', 'Plain'))
    const body = JSON.parse(fetchMock.mock.calls[0][1].body)
    expect(body.messages[0].extra).toBeUndefined()
  })

  /// A failed attempt is about this request, not about the conversation; saving
  /// it would resurrect the error on every later read.
  it('does not store a message that failed', async () => {
    const fetchMock = vi.fn().mockResolvedValue(jsonResponse(200, {}))
    vi.stubGlobal('fetch', fetchMock)

    const withFailure = conversation('c1', 'Failed')
    withFailure.messages[1].error = 'upstream refused'
    await new ServerBackend().save(withFailure)

    const body = JSON.parse(fetchMock.mock.calls[0][1].body)
    expect(body.messages).toHaveLength(1)
    expect(body.messages[0].role).toBe('user')
  })

  it('escapes an id that would otherwise change the path', async () => {
    const fetchMock = vi.fn().mockResolvedValue(jsonResponse(200, {}))
    vi.stubGlobal('fetch', fetchMock)

    await new ServerBackend().remove('../conversations')
    expect(fetchMock.mock.calls[0][0]).toBe('/api/conversations/..%2Fconversations')
  })

  it('answers with nothing for a conversation that is not there', async () => {
    vi.stubGlobal('fetch', vi.fn().mockResolvedValue(jsonResponse(404, { error: { message: 'no such conversation' } })))
    expect(await new ServerBackend().load('gone')).toBeNull()
  })

  it('treats deleting something already gone as success', async () => {
    vi.stubGlobal('fetch', vi.fn().mockResolvedValue(jsonResponse(404, {})))
    await expect(new ServerBackend().remove('gone')).resolves.toBeUndefined()
  })

  it('reports a store that is switched off as its own kind of answer', async () => {
    vi.stubGlobal('fetch', vi.fn().mockResolvedValue(jsonResponse(503, { error: { message: 'not configured' } })))
    await expect(new ServerBackend().list()).rejects.toBeInstanceOf(StoreUnavailable)
  })

  it('surfaces the reason a save was refused', async () => {
    vi.stubGlobal('fetch', vi.fn().mockResolvedValue(jsonResponse(400, {
      error: { message: 'that conversation belongs to someone else' },
    })))

    await expect(new ServerBackend().save(conversation('c1', 'Taken')))
      .rejects.toThrow('that conversation belongs to someone else')
  })

  it('falls back to the status when a failure carries no reason', async () => {
    vi.stubGlobal('fetch', vi.fn().mockResolvedValue(new Response('nope', { status: 500 })))
    await expect(new ServerBackend().list()).rejects.toThrow('500')
  })
})

describe('detectBackend', () => {
  beforeEach(() => vi.restoreAllMocks())

  it('uses the server when it says it keeps conversations', async () => {
    vi.stubGlobal('fetch', vi.fn().mockResolvedValue(jsonResponse(200, {
      user: { id: 'riley' }, multi_user: true, conversations: true,
    })))
    expect(await detectBackend()).toBeInstanceOf(ServerBackend)
  })

  it('keeps conversations in this browser when the server has nowhere to put them', async () => {
    vi.stubGlobal('fetch', vi.fn().mockResolvedValue(jsonResponse(200, {
      user: { id: 'local' }, multi_user: false, conversations: false,
    })))
    expect(await detectBackend()).toBeInstanceOf(LocalBackend)
  })

  /// The probe is a capability question, not a failed attempt: a deployment
  /// that keeps conversations locally must not log an error to say so.
  it('asks for capabilities rather than provoking a failure', async () => {
    const fetchMock = vi.fn().mockResolvedValue(jsonResponse(200, { conversations: false }))
    vi.stubGlobal('fetch', fetchMock)

    await detectBackend()
    expect(fetchMock).toHaveBeenCalledTimes(1)
    expect(fetchMock.mock.calls[0][0]).toBe('/api/me')
  })

  /// An unreachable API is the offline case: a cached page should still be able
  /// to show what this browser already had.
  it('keeps conversations in this browser when the server cannot be reached', async () => {
    vi.stubGlobal('fetch', vi.fn().mockRejectedValue(new TypeError('network error')))
    expect(await detectBackend()).toBeInstanceOf(LocalBackend)
  })

  /// Not signed in is not the same as no store: falling back would quietly show
  /// a different history than the one that belongs to this person.
  it('still uses the server when the caller is not signed in', async () => {
    vi.stubGlobal('fetch', vi.fn().mockResolvedValue(jsonResponse(401, { error: { message: 'unauthenticated' } })))
    expect(await detectBackend()).toBeInstanceOf(ServerBackend)
  })
})

describe('migrateLocalHistory', () => {
  beforeEach(() => {
    localStorage.clear()
    vi.restoreAllMocks()
  })

  function stubServer(saved: Conversation[] = []) {
    return {
      listIncludesMessages: false,
      list: vi.fn().mockResolvedValue([]),
      load: vi.fn().mockResolvedValue(null),
      save: vi.fn().mockImplementation(async (conversation: Conversation) => { saved.push(conversation) }),
      remove: vi.fn().mockResolvedValue(undefined),
      clear: vi.fn().mockResolvedValue(undefined),
    }
  }

  it('moves what this browser held to the server', async () => {
    const local = new LocalBackend()
    await local.save(conversation('c1', 'First'))
    await local.save(conversation('c2', 'Second'))
    const saved: Conversation[] = []

    expect(await migrateLocalHistory(stubServer(saved))).toBe(2)
    expect(saved.map(item => item.id).sort()).toEqual(['c1', 'c2'])
  })

  /// A migration that ran twice would resurrect conversations the user has since
  /// deleted on the server.
  it('runs once and then never again', async () => {
    await new LocalBackend().save(conversation('c1', 'First'))

    expect(await migrateLocalHistory(stubServer())).toBe(1)
    expect(await migrateLocalHistory(stubServer())).toBe(0)
  })

  it('leaves the local copy alone, in case the upload was not the end of it', async () => {
    const local = new LocalBackend()
    await local.save(conversation('c1', 'First'))

    await migrateLocalHistory(stubServer())
    expect((await local.list()).map(item => item.id)).toEqual(['c1'])
  })

  it('has nothing to do for a browser with no history', async () => {
    const server = stubServer()
    expect(await migrateLocalHistory(server)).toBe(0)
    expect(server.save).not.toHaveBeenCalled()
  })

  /// Stopping short leaves the flag unset, so the next load picks up where this
  /// one gave up rather than abandoning what did not make it.
  it('stops at the first refusal and tries again next time', async () => {
    const local = new LocalBackend()
    await local.save(conversation('c1', 'First'))
    await local.save(conversation('c2', 'Second'))

    const failing = stubServer()
    failing.save = vi.fn()
      .mockResolvedValueOnce(undefined)
      .mockRejectedValueOnce(new Error('the store is full'))

    expect(await migrateLocalHistory(failing)).toBe(1)
    expect(await migrateLocalHistory(stubServer())).toBe(2)
  })

  it('marks the job done without moving anything when told to', async () => {
    await new LocalBackend().save(conversation('c1', 'First'))
    markMigrated()

    const server = stubServer()
    expect(await migrateLocalHistory(server)).toBe(0)
    expect(server.save).not.toHaveBeenCalled()
  })
})
