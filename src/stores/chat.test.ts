import { beforeEach, describe, expect, it, vi } from 'vitest'
import { createPinia, setActivePinia } from 'pinia'
import { useChatStore } from './chat'

describe('chat store', () => {
  beforeEach(() => { localStorage.clear(); setActivePinia(createPinia()); vi.stubGlobal('crypto', { randomUUID: () => Math.random().toString(36) }) })
  it('creates and removes conversations', () => { const store = useChatStore(); const chat = store.createConversation('A useful title'); expect(store.activeId).toBe(chat.id); expect(chat.title).toBe('A useful title'); store.deleteConversation(chat.id); expect(store.conversations).toHaveLength(0) })
  it('ignores empty messages', async () => { const store = useChatStore(); await store.sendMessage('   '); expect(store.conversations).toHaveLength(0) })
})
