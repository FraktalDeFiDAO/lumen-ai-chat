<script setup lang="ts">
import { ref } from 'vue'
import { useRouter } from 'vue-router'
import { MessageSquare, PanelLeftClose, Search, Settings, Sparkles, Trash2, Plus } from 'lucide-vue-next'
import { useChatStore } from '@/stores/chat'

defineProps<{ open: boolean }>()
const emit = defineEmits<{ close: [] }>()
const store = useChatStore()
const router = useRouter()
const query = ref('')

function newChat() { store.selectConversation(null); router.push('/'); emit('close') }
function openChat(id: string) { store.selectConversation(id); router.push(`/chat/${id}`); emit('close') }
</script>

<template>
  <div v-if="open" class="fixed inset-0 z-30 bg-black/25 backdrop-blur-[1px] lg:hidden" @click="emit('close')" />
  <aside :class="['sidebar', open ? 'translate-x-0' : '-translate-x-full lg:translate-x-0']">
    <div class="flex items-center justify-between px-3 py-3">
      <button class="brand" @click="newChat"><span class="brand-mark"><Sparkles :size="17" /></span><span>Lumen</span></button>
      <button class="icon-button lg:hidden" aria-label="Close sidebar" @click="emit('close')"><PanelLeftClose :size="18" /></button>
    </div>
    <div class="px-3 pb-3"><button class="new-chat" @click="newChat"><Plus :size="17" /><span>New chat</span><kbd>⌘ K</kbd></button></div>
    <div class="relative mx-3 mb-3"><Search class="absolute left-3 top-2.5 text-stone-400" :size="15" /><input v-model="query" class="search-input" placeholder="Search conversations" /></div>
    <div class="sidebar-scroll">
      <p class="section-label">Recent</p>
      <div v-if="!store.sortedConversations.length" class="empty-history">Your conversations will appear here.</div>
      <button v-for="chat in store.sortedConversations.filter(c => c.title.toLowerCase().includes(query.toLowerCase()))" :key="chat.id" :class="['history-row group', { active: chat.id === store.activeId }]" @click="openChat(chat.id)">
        <MessageSquare :size="15" class="shrink-0" /><span class="min-w-0 flex-1 truncate text-left">{{ chat.title }}</span>
        <span class="row-delete" role="button" aria-label="Delete conversation" @click.stop="store.deleteConversation(chat.id)"><Trash2 :size="14" /></span>
      </button>
    </div>
    <div class="sidebar-footer">
      <button class="profile-row" @click="router.push('/settings')"><span class="avatar">A</span><span class="min-w-0 flex-1 text-left"><strong>Alex Morgan</strong><small>Free plan</small></span><Settings :size="17" /></button>
    </div>
  </aside>
</template>
