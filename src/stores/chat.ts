import { computed, ref, watch } from 'vue'
import { defineStore } from 'pinia'
import type { Conversation, Message } from '@/types/chat'

const STORAGE_KEY = 'lumen-conversations-v1'
const uid = () => crypto.randomUUID()

function loadConversations(): Conversation[] {
  try { return JSON.parse(localStorage.getItem(STORAGE_KEY) ?? '[]') as Conversation[] }
  catch { return [] }
}

const responses = [
  'I can help with that. A strong way to approach it is to separate the goal into three parts: what you know, what you need to decide, and the smallest useful next step. Start with the constraint that matters most, then test your assumptions before adding complexity.',
  `Here’s a concise framework:

1. Define the outcome in one sentence.
2. List the two biggest constraints.
3. Build the simplest version that proves the core idea.
4. Review the result, then iterate with evidence.

This keeps the work focused while leaving room to improve the details.`,
  'The key tradeoff is usually speed versus flexibility. If this is an early exploration, optimize for a clear, reversible decision. If it is production work, spend more time on reliability, observability, and failure states. Tell me which stage you’re in and I’ll make this more specific.',
]

export const useChatStore = defineStore('chat', () => {
  const conversations = ref<Conversation[]>(loadConversations())
  const activeId = ref<string | null>(null)
  const isGenerating = ref(false)
  const activeConversation = computed(() => conversations.value.find(c => c.id === activeId.value) ?? null)
  const sortedConversations = computed(() => [...conversations.value].sort((a, b) => b.updatedAt - a.updatedAt))
  watch(conversations, value => localStorage.setItem(STORAGE_KEY, JSON.stringify(value)), { deep: true })

  function createConversation(firstMessage?: string) {
    const now = Date.now()
    const conversation: Conversation = { id: uid(), title: firstMessage ? firstMessage.slice(0, 42) : 'New conversation', messages: [], createdAt: now, updatedAt: now }
    conversations.value.unshift(conversation)
    activeId.value = conversation.id
    return conversation
  }
  function selectConversation(id: string | null) { activeId.value = id }
  function deleteConversation(id: string) {
    conversations.value = conversations.value.filter(c => c.id !== id)
    if (activeId.value === id) activeId.value = null
  }
  function clearAll() { conversations.value = []; activeId.value = null }
  function addMessage(conversationId: string, role: Message['role'], content: string) {
    const conversation = conversations.value.find(c => c.id === conversationId)
    if (!conversation) return
    conversation.messages.push({ id: uid(), role, content, createdAt: Date.now() })
    conversation.updatedAt = Date.now()
    if (role === 'user' && conversation.messages.filter(m => m.role === 'user').length === 1) conversation.title = content.slice(0, 42)
  }
  let generationId = 0
  async function sendMessage(content: string) {
    const text = content.trim()
    if (!text || isGenerating.value) return activeId.value
    const conversation = activeConversation.value ?? createConversation(text)
    addMessage(conversation.id, 'user', text)
    isGenerating.value = true
    const requestId = ++generationId
    await new Promise(resolve => setTimeout(resolve, 650))
    if (requestId !== generationId) return conversation.id
    const reply = responses[conversation.messages.length % responses.length]!
    addMessage(conversation.id, 'assistant', reply)
    isGenerating.value = false
    return conversation.id
  }
  function stopGenerating() { generationId++; isGenerating.value = false }
  return { conversations, activeId, activeConversation, sortedConversations, isGenerating, createConversation, selectConversation, deleteConversation, clearAll, sendMessage, stopGenerating }
})
