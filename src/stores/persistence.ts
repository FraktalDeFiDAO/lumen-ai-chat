import type { Conversation, Message } from '@/types/chat'

/// Where conversations are kept.
///
/// Two answers, and the store must not care which it got. A single-user
/// deployment keeps them in this browser, exactly as it always has. A hosted one
/// keeps them on the server under the identity a proxy asserted, so they survive
/// a cleared cache and follow the person rather than the machine.
///
/// Which one is in use is discovered rather than configured: the client asks the
/// server for its history, and a server that has nowhere to put one says so.
export interface ConversationBackend {
  /// Whether transcripts arrive with the list. The local backend has them all
  /// already; the server sends summaries and fills a transcript in on demand,
  /// because a history of hundreds is a list of titles, not megabytes of chat.
  readonly listIncludesMessages: boolean
  list(): Promise<Conversation[]>
  load(id: string): Promise<Conversation | null>
  save(conversation: Conversation): Promise<void>
  remove(id: string): Promise<void>
  clear(): Promise<void>
}

const STORAGE_KEY = 'lumen-conversations-v1'

export class LocalBackend implements ConversationBackend {
  readonly listIncludesMessages = true

  async list(): Promise<Conversation[]> {
    return read()
  }

  async load(id: string): Promise<Conversation | null> {
    return read().find(conversation => conversation.id === id) ?? null
  }

  /// The whole set is rewritten because that is what `localStorage` offers; the
  /// cost is the size of one browser's chat history.
  async save(conversation: Conversation): Promise<void> {
    const all = read()
    const index = all.findIndex(item => item.id === conversation.id)
    if (index >= 0) all[index] = conversation
    else all.unshift(conversation)
    write(all)
  }

  async remove(id: string): Promise<void> {
    write(read().filter(conversation => conversation.id !== id))
  }

  async clear(): Promise<void> {
    write([])
  }
}

function read(): Conversation[] {
  try {
    const value = JSON.parse(localStorage.getItem(STORAGE_KEY) ?? '[]')
    return Array.isArray(value) ? value : []
  } catch { return [] }
}

function write(conversations: Conversation[]) {
  try { localStorage.setItem(STORAGE_KEY, JSON.stringify(conversations)) }
  catch { /* a full or disabled store is not worth failing a chat over */ }
}

/// The shape the server keeps. Everything the UI holds that the server has no
/// opinion about travels in `extra`, so adding a field to a message needs no
/// migration on either side.
interface StoredMessage {
  id: string
  role: 'user' | 'assistant'
  content: string
  created_at: number
  feedback?: string | null
  extra?: { tools?: Message['tools']; contextRecords?: number; incomplete?: boolean }
}

interface StoredConversation {
  id: string
  title: string
  created_at: number
  updated_at: number
  messages: StoredMessage[]
}

interface StoredSummary {
  id: string
  title: string
  created_at: number
  updated_at: number
  message_count: number
}

export class StoreUnavailable extends Error {}

const MIGRATED_KEY = 'lumen-conversations-migrated-v1'

/// Move this browser's conversations to the server, once.
///
/// A deployment that switches storage on would otherwise strand every
/// conversation its users already had: still in `localStorage`, no longer where
/// the app looks. This runs when the server is in use and has nothing for this
/// person yet, uploads what the browser holds, and records that it is done.
///
/// The local copy is left alone. It costs a few kilobytes and it is the only
/// thing standing between a failed upload and lost history.
export async function migrateLocalHistory(server: ConversationBackend): Promise<number> {
  if (readFlag(MIGRATED_KEY)) return 0

  const local = await new LocalBackend().list()
  if (!local.length) {
    writeFlag(MIGRATED_KEY)
    return 0
  }

  let moved = 0
  for (const conversation of local) {
    try {
      await server.save(conversation)
      moved += 1
    } catch {
      // One conversation the server will not take must not strand the rest, and
      // must not mark the migration done: the next load tries again.
      return moved
    }
  }
  writeFlag(MIGRATED_KEY)
  return moved
}

/// Record that there is nothing left to move, without moving anything: the
/// server already holds this person's history, so the browser's copy is a stale
/// duplicate rather than the only one.
export function markMigrated() {
  writeFlag(MIGRATED_KEY)
}

function readFlag(key: string): boolean {
  try { return localStorage.getItem(key) === 'true' }
  catch { return true }
}

function writeFlag(key: string) {
  try { localStorage.setItem(key, 'true') }
  catch { /* nothing to do; the worst case is trying again next load */ }
}

export class ServerBackend implements ConversationBackend {
  readonly listIncludesMessages = false

  async list(): Promise<Conversation[]> {
    const payload = await request<{ conversations: StoredSummary[] }>('/api/conversations')
    // A summary becomes a conversation with no messages yet; opening it is what
    // fetches the transcript. The sidebar only ever needed the title.
    return payload.conversations.map(summary => ({
      id: summary.id,
      title: summary.title,
      messages: [],
      createdAt: summary.created_at,
      updatedAt: summary.updated_at,
    }))
  }

  async load(id: string): Promise<Conversation | null> {
    const response = await fetch(`/api/conversations/${encodeURIComponent(id)}`)
    if (response.status === 404) return null
    return fromStored(await unwrap<StoredConversation>(response))
  }

  async save(conversation: Conversation): Promise<void> {
    await unwrap(await fetch(`/api/conversations/${encodeURIComponent(conversation.id)}`, {
      method: 'PUT',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify(toStored(conversation)),
    }))
  }

  async remove(id: string): Promise<void> {
    const response = await fetch(`/api/conversations/${encodeURIComponent(id)}`, { method: 'DELETE' })
    // Deleting something that is already gone is the outcome the caller wanted.
    if (response.status === 404) return
    await unwrap(response)
  }

  async clear(): Promise<void> {
    await unwrap(await fetch('/api/conversations', { method: 'DELETE' }))
  }
}

function toStored(conversation: Conversation): StoredConversation {
  return {
    id: conversation.id,
    title: conversation.title,
    created_at: conversation.createdAt,
    updated_at: conversation.updatedAt,
    messages: conversation.messages
      // A message that failed carries no answer worth keeping, and the error
      // text is about this attempt rather than about the conversation.
      .filter(message => !message.error)
      .map(message => ({
        id: message.id,
        role: message.role,
        content: message.content,
        created_at: message.createdAt,
        feedback: message.feedback ?? null,
        extra: pruned({
          tools: message.tools,
          contextRecords: message.contextRecords,
          incomplete: message.incomplete,
        }),
      })),
  }
}

function fromStored(stored: StoredConversation): Conversation {
  return {
    id: stored.id,
    title: stored.title,
    createdAt: stored.created_at,
    updatedAt: stored.updated_at,
    messages: stored.messages.map(message => ({
      id: message.id,
      role: message.role,
      content: message.content,
      createdAt: message.created_at,
      feedback: (message.feedback as Message['feedback']) ?? null,
      ...(message.extra?.tools ? { tools: message.extra.tools } : {}),
      ...(message.extra?.contextRecords !== undefined ? { contextRecords: message.extra.contextRecords } : {}),
      ...(message.extra?.incomplete ? { incomplete: true } : {}),
    })),
  }
}

/// Drop the keys that carry nothing, so a plain message stores nothing extra.
function pruned<T extends Record<string, unknown>>(value: T): T | undefined {
  const entries = Object.entries(value).filter(([, item]) => item !== undefined && item !== null)
  return entries.length ? (Object.fromEntries(entries) as T) : undefined
}

async function request<T>(url: string): Promise<T> {
  return unwrap<T>(await fetch(url))
}

/// A server with nowhere to keep conversations is not an error to show; it is
/// the signal to keep them in this browser instead.
async function unwrap<T>(response: Response): Promise<T> {
  if (response.status === 503) throw new StoreUnavailable('conversations are not stored on the server')
  if (!response.ok) {
    let detail = ''
    try {
      const payload = await response.json() as { error?: { message?: string } }
      detail = payload.error?.message ?? ''
    } catch { /* not JSON */ }
    throw new Error(detail || `The conversation store answered ${response.status}.`)
  }
  return response.status === 204 ? (undefined as T) : await response.json() as T
}

/// Ask the server whether it keeps conversations, and fall back to this browser
/// when it does not — including when it cannot be reached at all, so a chat
/// still works offline against a cached page.
///
/// This asks `/api/me` rather than trying the store and reading a 503 off the
/// floor: on a deployment that keeps conversations in the browser, that 503 is
/// the expected answer, and every browser would log a failed request for it on
/// every page load.
export async function detectBackend(): Promise<ConversationBackend> {
  try {
    const response = await fetch('/api/me')
    // Not signed in is not the same as no store; falling back would quietly
    // show a different history than the one that belongs to this person.
    if (response.status === 401) return new ServerBackend()
    if (response.ok) {
      const payload = await response.json() as { conversations?: boolean }
      return payload.conversations ? new ServerBackend() : new LocalBackend()
    }
  } catch { /* unreachable: keep working locally */ }
  return new LocalBackend()
}
