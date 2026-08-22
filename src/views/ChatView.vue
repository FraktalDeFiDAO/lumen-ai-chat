<script setup lang="ts">
import { computed, nextTick, onMounted, ref, watch } from 'vue'
import { useRoute, useRouter } from 'vue-router'
import { BookOpen, Code2, Lightbulb, Menu, MoreHorizontal, PenLine, Share2, Sparkles } from 'lucide-vue-next'
import AppSidebar from '@/components/AppSidebar.vue'
import ChatComposer from '@/components/ChatComposer.vue'
import MessageBubble from '@/components/MessageBubble.vue'
import { useChatStore } from '@/stores/chat'

const store = useChatStore(); const route = useRoute(); const router = useRouter()
const sidebarOpen = ref(false); const messagesEl = ref<HTMLElement | null>(null)
const suggestions = [
  { icon: PenLine, title: 'Help me write', text: 'Draft a thoughtful email to my team' },
  { icon: Lightbulb, title: 'Think through an idea', text: 'Help me pressure-test a product idea' },
  { icon: Code2, title: 'Build something', text: 'Create a plan for a small web app' },
  { icon: BookOpen, title: 'Learn something', text: 'Explain a complex topic simply' },
]
const hasMessages = computed(() => !!store.activeConversation?.messages.length)
async function send(text: string) { const id = await store.sendMessage(text); if (id && route.params.id !== id) router.replace(`/chat/${id}`) }
function syncRoute() { const id = typeof route.params.id === 'string' ? route.params.id : null; store.selectConversation(store.conversations.some(c => c.id === id) ? id : null) }
watch(() => route.params.id, syncRoute, { immediate: true })
watch(() => store.activeConversation?.messages.length, async () => { await nextTick(); messagesEl.value?.scrollTo({ top: messagesEl.value.scrollHeight, behavior: 'smooth' }) })
onMounted(() => window.addEventListener('keydown', e => { if ((e.metaKey || e.ctrlKey) && e.key.toLowerCase() === 'k') { e.preventDefault(); store.selectConversation(null); router.push('/') } }))
</script>

<template>
  <div class="app-shell"><AppSidebar :open="sidebarOpen" @close="sidebarOpen = false" />
    <main class="main-panel">
      <header class="topbar"><button class="icon-button lg:hidden" aria-label="Open sidebar" @click="sidebarOpen = true"><Menu :size="20" /></button><div class="topbar-title"><span>{{ store.activeConversation?.title ?? 'New chat' }}</span><small v-if="hasMessages">Lumen 2</small></div><div class="ml-auto flex items-center gap-1"><button v-if="hasMessages" class="share-button"><Share2 :size="16" /><span>Share</span></button><button class="icon-button" aria-label="More options"><MoreHorizontal :size="20" /></button></div></header>
      <section v-if="!hasMessages" class="welcome">
        <div class="welcome-inner"><div class="welcome-mark"><Sparkles :size="23" /></div><h1>How can I help you today?</h1><p>Ask a question, explore an idea, or start creating.</p>
          <div class="suggestion-grid"><button v-for="item in suggestions" :key="item.title" class="suggestion-card" @click="send(item.text)"><component :is="item.icon" :size="19" /><span><strong>{{ item.title }}</strong><small>{{ item.text }}</small></span></button></div>
        </div>
        <div class="welcome-composer"><ChatComposer :generating="store.isGenerating" @send="send" @stop="store.stopGenerating" /><p class="disclaimer">Lumen can make mistakes. Check important information.</p></div>
      </section>
      <section v-else ref="messagesEl" class="conversation"><div class="message-list"><MessageBubble v-for="message in store.activeConversation!.messages" :key="message.id" :message="message" /><div v-if="store.isGenerating" class="thinking"><span /><span /><span /></div></div></section>
      <div v-if="hasMessages" class="conversation-composer"><div class="composer-wrap"><ChatComposer :generating="store.isGenerating" @send="send" @stop="store.stopGenerating" /><p class="disclaimer">Lumen can make mistakes. Check important information.</p></div></div>
    </main>
  </div>
</template>
