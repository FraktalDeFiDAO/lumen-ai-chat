import { computed, ref, watch } from 'vue'
import { defineStore } from 'pinia'
import type { ChatModel, Conversation, Feedback, Message, ToolActivity } from '@/types/chat'
import { useSettingsStore } from './settings'
import { LocalBackend, ServerBackend, detectBackend, markMigrated, migrateLocalHistory, type ConversationBackend } from './persistence'

const MODEL_KEY = 'lumen-model-v1'
/// A streamed answer changes the conversation on every chunk. Saving each one
/// would be a request per token; waiting for the stream to end would lose the
/// answer if the tab closed. This is the compromise, and it is why `flush`
/// exists for the moments that must not wait.
const SAVE_DEBOUNCE_MS = 700
const uid = () => crypto.randomUUID()

/// The payload of one SSE block: every `data:` line, rejoined.
function blockData(block: string): string {
  return block.split(/\r?\n/)
    .filter(line => line.startsWith('data:'))
    .map(line => line.slice(5).trimStart())
    .join('\n')
}

/// Read an SSE body block by block. `onBlock` returns true when the stream is
/// over, which is what `[DONE]` means on every endpoint this app talks to.
async function readFrames(response: Response, onBlock: (block: string) => boolean): Promise<boolean> {
  if (!response.body) throw new Error('The server returned an empty response.')
  const reader = response.body.getReader()
  const decoder = new TextDecoder()
  let buffer = ''
  let finished = false
  while (!finished) {
    const { done, value } = await reader.read()
    buffer += decoder.decode(value, { stream: !done })
    const blocks = buffer.split(/\r?\n\r?\n/)
    buffer = blocks.pop() ?? ''
    for (const block of blocks) {
      if (onBlock(block)) { finished = true; break }
    }
    if (done) break
  }
  // A stream cut off mid-frame still carries whatever the last block held.
  if (buffer.trim() && !finished) finished = onBlock(buffer)
  return finished
}

export function parseSSEBlock(block: string): { done: boolean; content: string; error?: string } {
  const data = blockData(block)
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

/// Returns whether the stream said it had finished.
///
/// A body that simply stops is what a proxy timing out mid-response looks like:
/// no error to catch, no way to tell a finished answer from a truncated one
/// except that `[DONE]` never arrived.
export async function readSSE(response: Response, onContent: (content: string) => void): Promise<boolean> {
  return readFrames(response, block => {
    const event = parseSSEBlock(block)
    if (event.error) throw new Error(event.error)
    if (event.content) onContent(event.content)
    return event.done
  })
}

/// One frame from `/api/agent`. The harness tags each by `event`; anything
/// unrecognised is ignored rather than shown, so a newer server adding a frame
/// type cannot break an older page.
export interface HarnessFrame {
  event?: string
  step?: number
  id?: string
  name?: string
  arguments?: unknown
  output?: string
  failed?: boolean
  text?: string
  records?: number
  tool_calls?: number
  stopped_at_step_limit?: boolean
}

export function parseHarnessBlock(block: string): { done: boolean; frame?: HarnessFrame; error?: string } {
  const data = blockData(block)
  if (!data) return { done: false }
  if (data.trim() === '[DONE]') return { done: true }
  try {
    const value = JSON.parse(data) as HarnessFrame & { error?: { message?: unknown } | string }
    if (value.error) {
      const message = typeof value.error === 'string' ? value.error : value.error.message
      return { done: false, error: typeof message === 'string' ? message : 'The agent harness returned an error.' }
    }
    return { done: false, frame: value }
  } catch { return { done: false } }
}

export async function readHarnessSSE(response: Response, onFrame: (frame: HarnessFrame) => void): Promise<boolean> {
  return readFrames(response, block => {
    const event = parseHarnessBlock(block)
    if (event.error) throw new Error(event.error)
    if (event.frame) onFrame(event.frame)
    return event.done
  })
}

/// Fold one harness frame into the message being built.
///
/// Text arrives per step and is joined the same way the harness joins it for
/// its own outcome, so the transcript a user reads matches the one the model
/// was given on the next turn.
export function applyHarnessFrame(message: Message, frame: HarnessFrame) {
  switch (frame.event) {
    case 'context':
      message.contextRecords = frame.records
      break
    case 'text':
      if (frame.text) message.content += message.content ? `\n\n${frame.text}` : frame.text
      break
    case 'tool_call': {
      const activity: ToolActivity = { id: frame.id ?? '', name: frame.name ?? 'tool', arguments: frame.arguments }
      message.tools = [...(message.tools ?? []), activity]
      break
    }
    case 'tool_result': {
      const activity = message.tools?.find(tool => tool.id === frame.id)
      if (activity) { activity.output = frame.output; activity.failed = frame.failed === true }
      break
    }
    case 'outcome':
      if (!message.content && typeof frame.text === 'string') message.content = frame.text
      if (frame.stopped_at_step_limit) message.incomplete = true
      break
  }
}

export const useChatStore = defineStore('chat', () => {
  const preferences = useSettingsStore()
  const conversations = ref<Conversation[]>([])
  /// Where conversations are kept. Local until the server says otherwise, so a
  /// page that never reaches the API still works.
  const backend = ref<ConversationBackend>(new LocalBackend())
  const historyError = ref('')
  /// Ids whose transcripts are in hand. The server sends a list of titles, so
  /// opening a conversation is what fetches what was said in it.
  const loaded = new Set<string>()
  const activeId = ref<string | null>(null)
  const isGenerating = ref(false)
  const models = ref<ChatModel[]>([])
  const selectedModel = ref(localStorage.getItem(MODEL_KEY) ?? '')
  const modelsError = ref('')
  const activeConversation = computed(() => conversations.value.find(c => c.id === activeId.value) ?? null)
  const sortedConversations = computed(() => [...conversations.value].sort((a, b) => b.updatedAt - a.updatedAt))
  // Persist whichever conversation is being changed. Only the open one ever
  // mutates -- streaming, feedback, regeneration all happen there -- so watching
  // it is enough, and it keeps a save from rewriting the whole history.
  watch(activeConversation, value => { if (value) schedule(value) }, { deep: true })
  watch(selectedModel, value => value ? localStorage.setItem(MODEL_KEY, value) : localStorage.removeItem(MODEL_KEY))

  let controller: AbortController | null = null

  const pending = new Map<string, ReturnType<typeof setTimeout>>()

  function schedule(conversation: Conversation) {
    const existing = pending.get(conversation.id)
    if (existing) clearTimeout(existing)
    pending.set(conversation.id, setTimeout(() => {
      pending.delete(conversation.id)
      void save(conversation)
    }, SAVE_DEBOUNCE_MS))
  }

  async function save(conversation: Conversation) {
    try {
      await backend.value.save(conversation)
      historyError.value = ''
    } catch (error) {
      historyError.value = error instanceof Error ? error.message : 'Could not save this conversation.'
    }
  }

  /// Write everything that is waiting, now. Called when a run finishes and
  /// before anything that would make a pending save meaningless.
  async function flush() {
    const waiting = [...pending.keys()]
    for (const id of waiting) {
      const timer = pending.get(id)
      if (timer) clearTimeout(timer)
      pending.delete(id)
      const conversation = conversations.value.find(item => item.id === id)
      if (conversation) await save(conversation)
    }
  }

  /// The history is loaded once per session, whichever page asks first.
  ///
  /// It used to be read synchronously when the store was constructed, so every
  /// view simply had it. Now that it comes over the network, a view that forgets
  /// to ask sees an empty history -- which is how Data controls came to report
  /// nothing to delete when it was opened directly.
  let loading: Promise<void> | null = null

  function ensureHistory() {
    loading ??= loadHistory()
    return loading
  }

  /// Discover where conversations live and read the history from there.
  async function loadHistory() {
    historyError.value = ''
    try {
      backend.value = await detectBackend()
      let listed = await backend.value.list()

      // A deployment that has just switched storage on would otherwise strand
      // whatever this browser already held. Only when the server has nothing
      // for this person: an empty history there and a full one here is the one
      // situation where the browser's copy is the real one.
      if (backend.value instanceof ServerBackend) {
        if (listed.length) markMigrated()
        else if (await migrateLocalHistory(backend.value)) listed = await backend.value.list()
      }

      conversations.value = listed
      loaded.clear()
      if (backend.value.listIncludesMessages) {
        for (const conversation of conversations.value) loaded.add(conversation.id)
      }
    } catch (error) {
      historyError.value = error instanceof Error ? error.message : 'Could not load your conversations.'
    }
  }

  /// Fetch a transcript the history list did not carry. Selecting a conversation
  /// is the only thing that needs one, so it is the only thing that waits.
  async function ensureLoaded(id: string) {
    if (loaded.has(id)) return
    try {
      const conversation = await backend.value.load(id)
      const index = conversations.value.findIndex(item => item.id === id)
      if (conversation && index >= 0) conversations.value[index] = conversation
      loaded.add(id)
    } catch (error) {
      historyError.value = error instanceof Error ? error.message : 'Could not open that conversation.'
    }
  }

  function createConversation(firstMessage?: string) {
    const now = Date.now()
    const conversation: Conversation = { id: uid(), title: firstMessage ? firstMessage.slice(0, 42) : 'New conversation', messages: [], createdAt: now, updatedAt: now }
    conversations.value.unshift(conversation)
    activeId.value = conversation.id
    loaded.add(conversation.id)
    // The reactive element rather than the object that was pushed: everything
    // that follows mutates this conversation, and through the raw object those
    // writes never reach the watcher that saves it.
    return conversations.value[0]
  }
  function selectConversation(id: string | null) {
    activeId.value = id
    if (id) void ensureLoaded(id)
  }
  function deleteConversation(id: string) {
    if (activeId.value === id) stopGenerating()
    const timer = pending.get(id)
    if (timer) { clearTimeout(timer); pending.delete(id) }
    conversations.value = conversations.value.filter(c => c.id !== id)
    loaded.delete(id)
    if (activeId.value === id) activeId.value = null
    void backend.value.remove(id).catch(() => { historyError.value = 'Could not delete that conversation.' })
  }
  function clearAll() {
    stopGenerating()
    for (const timer of pending.values()) clearTimeout(timer)
    pending.clear()
    conversations.value = []
    loaded.clear()
    activeId.value = null
    void backend.value.clear().catch(() => { historyError.value = 'Could not clear your conversations.' })
  }
  function addMessage(conversation: Conversation, role: Message['role'], content: string) {
    const message: Message = { id: uid(), role, content, createdAt: Date.now(), feedback: null }
    conversation.messages.push(message)
    conversation.updatedAt = Date.now()
    if (role === 'user' && conversation.messages.filter(m => m.role === 'user').length === 1) conversation.title = content.split('\n')[0].slice(0, 42)
    // The reactive element, not the object that was pushed. Streaming mutates
    // this message for as long as the answer takes; through the raw object those
    // writes are invisible to the watcher that persists the conversation, so a
    // reload would show an answer that is empty or cut off at whichever
    // millisecond `updatedAt` last happened to change.
    return conversation.messages[conversation.messages.length - 1]
  }

  async function loadModels() {
    modelsError.value = ''
    try {
      const response = await fetch('/api/models')
      if (!response.ok) throw new Error(`Unable to load models (${response.status})`)
      const data = await response.json() as { models?: ChatModel[]; default_ai_coder?: string; default_provider?: string; provider_errors?: Array<{ provider?: string; message?: string }> }
      models.value = Array.isArray(data.models) ? data.models.filter(model => model?.id && model?.key && model?.ai_coder && model?.provider && model?.name) : []
      if (data.provider_errors?.length) modelsError.value = `${data.provider_errors.length} provider${data.provider_errors.length === 1 ? '' : 's'} unavailable; fallback models are shown.`
      if (!selectedModel.value || !models.value.some(model => model.key === selectedModel.value)) {
        selectedModel.value = models.value.find(model => model.ai_coder === data.default_ai_coder && model.provider === data.default_provider)?.key ?? models.value[0]?.key ?? ''
      }
    } catch (error) {
      modelsError.value = error instanceof Error ? error.message : 'Unable to load models.'
    }
  }

  /// Run the model and write its answer into `into`, or into a new message.
  ///
  /// Continuing passes the message that was cut off, so the text is appended to
  /// what is already there rather than replacing it, and the transcript sent to
  /// the model ends with the partial answer -- without which it has nothing to
  /// continue from.
  async function generateAssistant(conversation: Conversation, into?: Message) {
    if (isGenerating.value) return conversation.id
    const assistant = into ?? addMessage(conversation, 'assistant', '')
    const continuing = into !== undefined
    if (continuing) {
      assistant.error = undefined
      assistant.incomplete = undefined
    }
    isGenerating.value = true
    controller = new AbortController()
    const currentController = controller
    try {
      const selected = models.value.find(model => model.key === selectedModel.value)
      const systemPrompt = [preferences.settings.systemPrompt.trim(), `Respond in ${preferences.settings.language}. Use a ${preferences.settings.responseStyle.toLowerCase()} level of detail.`].filter(Boolean).join('\n\n')
      // Agent mode runs the same conversation through the harness, which
      // executes tools against the workspace instead of only describing them.
      const agent = preferences.settings.agentMode
      const response = await fetch(agent ? '/api/agent' : '/api/chat', {
        method: 'POST',
        headers: { 'Content-Type': 'application/json', Accept: 'text/event-stream' },
        signal: currentController.signal,
        body: JSON.stringify({
          ...(selected ? { ai_coder: selected.ai_coder, provider: selected.provider, model: selected.key } : {}),
          temperature: preferences.settings.temperature,
          max_tokens: preferences.settings.maxTokens,
          messages: [
            { role: 'system', content: systemPrompt },
            ...conversation.messages
              .slice(0, conversation.messages.indexOf(assistant))
              .filter(message => !message.error || !!message.content)
              .map(({ role, content }) => ({ role, content })),
            ...(continuing ? [{ role: 'assistant' as const, content: assistant.content }] : []),
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
      if (agent) {
        const finished = await readHarnessSSE(response, frame => { applyHarnessFrame(assistant, frame); conversation.updatedAt = Date.now() })
        if (!finished) assistant.incomplete = true
        // A run that only called tools has no text, but it is not a failure --
        // the tools it ran are the answer, and they are on the message.
        if (!assistant.content && !assistant.tools?.length) throw new Error('The agent returned no response.')
        if (!assistant.content) assistant.content = 'No summary was returned; see the tool activity above.'
      } else {
        const finished = await readSSE(response, chunk => { assistant.content += chunk; conversation.updatedAt = Date.now() })
        if (!assistant.content) throw new Error('The model returned no response.')
        if (!finished) assistant.incomplete = true
      }
    } catch (error) {
      if (currentController.signal.aborted) {
        if (!assistant.content && !assistant.tools?.length) conversation.messages = conversation.messages.filter(message => message.id !== assistant.id)
      } else {
        assistant.error = error instanceof Error ? error.message : 'Something went wrong. Please try again.'
        // A stream that died partway still produced an answer. Marking it
        // incomplete rather than replacing it keeps the tokens the user paid
        // for and gives them something to continue from.
        if (assistant.content || assistant.tools?.length) assistant.incomplete = true
        else assistant.content = 'Unable to generate a response.'
      }
    } finally {
      if (controller === currentController) {
        controller = null
        isGenerating.value = false
      }
      conversation.updatedAt = Date.now()
    }
    await flush()
    return conversation.id
  }

  async function sendMessage(content: string) {
    const text = content.trim()
    if (!text || isGenerating.value) return activeId.value
    const conversation = activeConversation.value ?? createConversation(text)
    addMessage(conversation, 'user', text)
    return generateAssistant(conversation)
  }

  /// Ask the model to carry on from an answer that was cut off.
  async function continueAnswer(messageId: string) {
    if (isGenerating.value) return activeId.value
    const conversation = activeConversation.value
    const message = conversation?.messages.find(item => item.id === messageId)
    if (!conversation || !message || message.role !== 'assistant' || !message.content) {
      return activeId.value
    }
    return generateAssistant(conversation, message)
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

  return { conversations, activeId, activeConversation, sortedConversations, isGenerating, models, selectedModel, modelsError, historyError, backend, loadHistory, ensureHistory, ensureLoaded, flush, createConversation, selectConversation, deleteConversation, clearAll, sendMessage, stopGenerating, regenerate, continueAnswer, setFeedback, loadModels }
})
