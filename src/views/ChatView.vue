<script setup lang="ts">
import { computed, nextTick, onBeforeUnmount, onMounted, ref, watch } from 'vue'
import { useRoute, useRouter } from 'vue-router'
import { BookOpen, Check, Code2, Lightbulb, Menu, MoreHorizontal, PenLine, Share2, Sparkles, Trash2 } from 'lucide-vue-next'
import AppSidebar from '@/components/AppSidebar.vue'
import ChatComposer from '@/components/ChatComposer.vue'
import MessageBubble from '@/components/MessageBubble.vue'
import { useChatStore } from '@/stores/chat'

const store = useChatStore(); const route = useRoute(); const router = useRouter()
const sidebarOpen = ref(false); const messagesEl = ref<HTMLElement | null>(null); const overflowOpen = ref(false); const shareStatus = ref('')
const suggestions = [
  { icon: PenLine, title: 'Help me write', text: 'Draft a thoughtful email to my team' },
  { icon: Lightbulb, title: 'Think through an idea', text: 'Help me pressure-test a product idea' },
  { icon: Code2, title: 'Build something', text: 'Create a plan for a small web app' },
  { icon: BookOpen, title: 'Learn something', text: 'Explain a complex topic simply' },
]
const hasMessages = computed(() => !!store.activeConversation?.messages.length)
const modelName = computed(() => {
  const model = store.models.find(item => item.key === store.selectedModel)
  return model ? `${model.ai_coder_name} · ${model.provider_name} · ${model.name}` : 'Lumen'
})
async function send(text: string) { const id = await store.sendMessage(text); if (id && route.params.id !== id) await router.replace(`/chat/${id}`) }
function syncRoute() { const id = typeof route.params.id === 'string' ? route.params.id : null; store.selectConversation(store.conversations.some(c => c.id === id) ? id : null) }
watch(() => route.params.id, syncRoute, { immediate: true })
watch(() => store.activeConversation?.messages.map(message => message.content.length).join(','), async () => { await nextTick(); messagesEl.value?.scrollTo({ top: messagesEl.value.scrollHeight, behavior: 'smooth' }) })
function keyboardShortcut(event: KeyboardEvent) {
  if ((event.metaKey || event.ctrlKey) && event.key.toLowerCase() === 'k') { event.preventDefault(); store.selectConversation(null); void router.push('/') }
  if (event.key === 'Escape') overflowOpen.value = false
}
// The history arrives asynchronously now, so the route is resolved again once
// it is here: a deep link to /chat/:id is opened before there is anything to
// find it in, and would otherwise fall back to the empty state.
onMounted(async () => {
  window.addEventListener('keydown', keyboardShortcut)
  void store.loadModels()
  await store.ensureHistory()
  syncRoute()
})
onBeforeUnmount(() => window.removeEventListener('keydown', keyboardShortcut))
async function shareChat() {
  const conversation = store.activeConversation
  if (!conversation) return
  const text = conversation.messages.map(message => `${message.role === 'user' ? 'You' : 'Lumen'}: ${message.content}`).join('\n\n')
  const nativeShare = typeof navigator.share === 'function'
  try {
    if (nativeShare) await navigator.share({ title: conversation.title, text })
    else await navigator.clipboard.writeText(text)
    shareStatus.value = nativeShare ? 'Shared' : 'Copied link text'
  } catch (error) {
    if ((error as DOMException).name !== 'AbortError') shareStatus.value = 'Could not share'
  }
  setTimeout(() => shareStatus.value = '', 1800)
}
async function deleteCurrent() {
  const id = store.activeId
  overflowOpen.value = false
  if (id && confirm('Delete this conversation? This cannot be undone.')) { store.deleteConversation(id); await router.replace('/') }
}
</script>

<template>
  <div class="app-shell"><AppSidebar :open="sidebarOpen" @close="sidebarOpen = false" />
    <main class="main-panel">
      <header class="topbar"><button class="icon-button lg:hidden" aria-label="Open sidebar" @click="sidebarOpen = true"><Menu :size="20" /></button><div class="topbar-title"><span>{{ store.activeConversation?.title ?? 'New chat' }}</span><small v-if="hasMessages">{{ modelName }}</small></div><div class="ml-auto flex items-center gap-1"><span class="share-status" role="status">{{ shareStatus }}</span><button v-if="hasMessages" class="share-button" @click="shareChat"><Check v-if="shareStatus === 'Shared' || shareStatus === 'Copied link text'" :size="16" /><Share2 v-else :size="16" /><span>Share</span></button><div class="overflow-wrap"><button class="icon-button" aria-label="Chat options" aria-haspopup="menu" :aria-expanded="overflowOpen" @click="overflowOpen = !overflowOpen"><MoreHorizontal :size="20" /></button><div v-if="overflowOpen" class="overflow-menu" role="menu"><button v-if="hasMessages" role="menuitem" @click="deleteCurrent"><Trash2 :size="15" />Delete chat</button><span v-else>No chat options</span></div></div></div></header>
      <section v-if="!hasMessages" class="welcome">
        <div class="welcome-inner"><div class="welcome-mark"><Sparkles :size="23" /></div><h1>How can I help you today?</h1><p>Ask a question, explore an idea, or start creating.</p>
          <div class="suggestion-grid"><button v-for="item in suggestions" :key="item.title" class="suggestion-card" @click="send(item.text)"><component :is="item.icon" :size="19" /><span><strong>{{ item.title }}</strong><small>{{ item.text }}</small></span></button></div>
        </div>
        <div class="welcome-composer"><ChatComposer v-model:model="store.selectedModel" :models="store.models" :models-error="store.modelsError" :generating="store.isGenerating" @send="send" @stop="store.stopGenerating" /><p class="disclaimer">Lumen can make mistakes. Check important information.</p></div>
      </section>
      <section v-else ref="messagesEl" class="conversation"><div class="message-list"><MessageBubble v-for="message in store.activeConversation!.messages" :key="message.id" :message="message" :regenerating="store.isGenerating" @feedback="store.setFeedback" @regenerate="store.regenerate" /><div v-if="store.isGenerating && !store.activeConversation!.messages[store.activeConversation!.messages.length - 1]?.content" class="thinking" aria-label="Lumen is responding"><span /><span /><span /></div></div></section>
      <div v-if="hasMessages" class="conversation-composer"><div class="composer-wrap"><ChatComposer v-model:model="store.selectedModel" :models="store.models" :models-error="store.modelsError" :generating="store.isGenerating" @send="send" @stop="store.stopGenerating" /><p class="disclaimer">Lumen can make mistakes. Check important information.</p></div></div>
    </main>
  </div>
</template>
