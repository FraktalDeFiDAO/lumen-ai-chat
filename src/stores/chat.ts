import { computed, ref, watch } from 'vue'
import { defineStore } from 'pinia'
import type { ChatModel, Conversation, Feedback, Message } from '@/types/chat'
import { useSettingsStore } from './settings'

const STORAGE_KEY = 'lumen-conversations-v1'
const MODEL_KEY = 'lumen-model-v1'
const uid = () => crypto.randomUUID()

function loadConversations(): Conversation[] {
  try {
    const value = JSON.parse(localStorage.getItem(STORAGE_KEY) ?? '[]')
    return Array.isArray(value) ? value : []
  } catch { return [] }
}

export function parseSSEBlock(block: string): { done: boolean; content: string; error?: string } {
  const data = block.split(/\r?\n/)
    .filter(line => line.startsWith('data:'))
    .map(line => line.slice(5).trimStart())
    .join('\n')
  if (!data) return { done: false, content: '' }
  if (data.trim() === '[DONE]') return { done: true, content: '' }
  try {
    const event = JSON.parse(data) as { choices?: Array<{ delta?: { content?: unknown } }>; error?: { message?: unknown } | string }
    if (event.error) {
      const message = typeof event.error === 'string' ? event.error : event.error.message
      return { done: false, content: '', error: typeof message === 'string' ? message : 'The AI provider returned an error.' }
    }
    const content = event.choices?.[0]?.delta?.content
    return { done: false, content: typeof content === 'string' ? content : '' }
  } catch { return { done: false, content: '' } }
}

export async function readSSE(response: Response, onContent: (content: string) => void) {
  if (!response.body) throw new Error('The server returned an empty response.')
  const reader = response.body.getReader()
  const decoder = new TextDecoder()
  let buffer = ''
  let doneEvent = false
  while (!doneEvent) {
    const { done, value } = await reader.read()
    buffer += decoder.decode(value, { stream: !done })
    const blocks = buffer.split(/\r?\n\r?\n/)
    buffer = blocks.pop() ?? ''
    for (const block of blocks) {
      const event = parseSSEBlock(block)
      if (event.error) throw new Error(event.error)
      if (event.content) onContent(event.content)
      if (event.done) { doneEvent = true; break }
    }
    if (done) break
  }
  if (buffer.trim() && !doneEvent) {
    const event = parseSSEBlock(buffer)
    if (event.error) throw new Error(event.error)
    if (event.content) onContent(event.content)
  }
}

export const useChatStore = defineStore('chat', () => {
  const preferences = useSettingsStore()
  const conversations = ref<Conversation[]>(loadConversations())
  const activeId = ref<string | null>(null)
  const isGenerating = ref(false)
  const models = ref<ChatModel[]>([])
  const selectedModel = ref(localStorage.getItem(MODEL_KEY) ?? '')
  const modelsError = ref('')
  const activeConversation = computed(() => conversations.value.find(c => c.id === activeId.value) ?? null)
  const sortedConversations = computed(() => [...conversations.value].sort((a, b) => b.updatedAt - a.updatedAt))
  watch(conversations, value => localStorage.setItem(STORAGE_KEY, JSON.stringify(value)), { deep: true })
  watch(selectedModel, value => value ? localStorage.setItem(MODEL_KEY, value) : localStorage.removeItem(MODEL_KEY))

  let controller: AbortController | null = null

  function createConversation(firstMessage?: string) {
    const now = Date.now()
    const conversation: Conversation = { id: uid(), title: firstMessage ? firstMessage.slice(0, 42) : 'New conversation', messages: [], createdAt: now, updatedAt: now }
    conversations.value.unshift(conversation)
    activeId.value = conversation.id
    return conversation
  }
  function selectConversation(id: string | null) { activeId.value = id }
  function deleteConversation(id: string) {
    if (activeId.value === id) stopGenerating()
    conversations.value = conversations.value.filter(c => c.id !== id)
    if (activeId.value === id) activeId.value = null
  }
  function clearAll() { stopGenerating(); conversations.value = []; activeId.value = null }
  function addMessage(conversation: Conversation, role: Message['role'], content: string) {
    const message: Message = { id: uid(), role, content, createdAt: Date.now(), feedback: null }
    conversation.messages.push(message)
    conversation.updatedAt = Date.now()
    if (role === 'user' && conversation.messages.filter(m => m.role === 'user').length === 1) conversation.title = content.split('\n')[0]!.slice(0, 42)
    return message
  }

  async function loadModels() {
    modelsError.value = ''
    try {
      const response = await fetch('/api/models')
      if (!response.ok) throw new Error(`Unable to load models (${response.status})`)
      const data = await response.json() as { models?: ChatModel[]; default_provider?: string; provider_errors?: Array<{ provider?: string; message?: string }> }
      models.value = Array.isArray(data.models) ? data.models.filter(model => model?.id && model?.key && model?.provider && model?.name) : []
      if (data.provider_errors?.length) modelsError.value = `${data.provider_errors.length} provider${data.provider_errors.length === 1 ? '' : 's'} unavailable; fallback models are shown.`
      if (!selectedModel.value || !models.value.some(model => model.key === selectedModel.value)) {
        selectedModel.value = models.value.find(model => model.provider === data.default_provider)?.key ?? models.value[0]?.key ?? ''
      }
    } catch (error) {
      modelsError.value = error instanceof Error ? error.message : 'Unable to load models.'
    }
  }

  async function generateAssistant(conversation: Conversation) {
    if (isGenerating.value) return conversation.id
    const assistant = addMessage(conversation, 'assistant', '')
    isGenerating.value = true
    controller = new AbortController()
    const currentController = controller
    try {
      const selected = models.value.find(model => model.key === selectedModel.value)
      const systemPrompt = [preferences.settings.systemPrompt.trim(), `Respond in ${preferences.settings.language}. Use a ${preferences.settings.responseStyle.toLowerCase()} level of detail.`].filter(Boolean).join('\n\n')
      const response = await fetch('/api/chat', {
        method: 'POST',
        headers: { 'Content-Type': 'application/json', Accept: 'text/event-stream' },
        signal: currentController.signal,
        body: JSON.stringify({
          ...(selected ? { provider: selected.provider, model: selected.id } : {}),
          temperature: preferences.settings.temperature,
          max_tokens: preferences.settings.maxTokens,
          messages: [
            { role: 'system', content: systemPrompt },
            ...conversation.messages.slice(0, -1).filter(message => !message.error).map(({ role, content }) => ({ role, content })),
          ],
        }),
      })
      if (!response.ok) {
        let detail = ''
        try {
          const payload = await response.json() as { error?: string | { message?: string }; message?: string }
          detail = typeof payload.error === 'string' ? payload.error : payload.error?.message ?? payload.message ?? ''
        } catch { /* non-JSON error */ }
        throw new Error(detail || `Request failed (${response.status})`)
      }
      await readSSE(response, chunk => { assistant.content += chunk; conversation.updatedAt = Date.now() })
      if (!assistant.content) throw new Error('The model returned no response.')
    } catch (error) {
      if (currentController.signal.aborted) {
        if (!assistant.content) conversation.messages = conversation.messages.filter(message => message.id !== assistant.id)
      } else {
        assistant.error = error instanceof Error ? error.message : 'Something went wrong. Please try again.'
        if (!assistant.content) assistant.content = 'Unable to generate a response.'
      }
    } finally {
      if (controller === currentController) {
        controller = null
        isGenerating.value = false
      }
      conversation.updatedAt = Date.now()
    }
    return conversation.id
  }

  async function sendMessage(content: string) {
    const text = content.trim()
    if (!text || isGenerating.value) return activeId.value
    const conversation = activeConversation.value ?? createConversation(text)
    addMessage(conversation, 'user', text)
    return generateAssistant(conversation)
  }

  async function regenerate(messageId?: string) {
    if (isGenerating.value) return activeId.value
    const conversation = activeConversation.value
    if (!conversation) return null
    let index = messageId ? conversation.messages.findIndex(message => message.id === messageId) : -1
    if (index < 0) index = conversation.messages.map(message => message.role).lastIndexOf('assistant')
    if (index < 0) return conversation.id
    conversation.messages.splice(index)
    if (!conversation.messages.some(message => message.role === 'user')) return conversation.id
    return generateAssistant(conversation)
  }

  function stopGenerating() { controller?.abort(); controller = null; isGenerating.value = false }
  function setFeedback(messageId: string, feedback: Exclude<Feedback, null>) {
    const message = activeConversation.value?.messages.find(item => item.id === messageId)
    if (message) message.feedback = message.feedback === feedback ? null : feedback
  }

  return { conversations, activeId, activeConversation, sortedConversations, isGenerating, models, selectedModel, modelsError, createConversation, selectConversation, deleteConversation, clearAll, sendMessage, stopGenerating, regenerate, setFeedback, loadModels }
})
