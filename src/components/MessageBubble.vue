<script setup lang="ts">
import { ref } from 'vue'
import { Check, Clipboard, RotateCcw, ThumbsDown, ThumbsUp, Sparkles } from 'lucide-vue-next'
import type { Message } from '@/types/chat'
const props = defineProps<{ message: Message }>()
const copied = ref(false)
async function copy() { await navigator.clipboard.writeText(props.message.content); copied.value = true; setTimeout(() => copied.value = false, 1300) }
</script>

<template>
  <article :class="['message', message.role]">
    <div v-if="message.role === 'assistant'" class="assistant-mark"><Sparkles :size="16" /></div>
    <div class="message-body"><div class="message-content">{{ message.content }}</div>
      <div v-if="message.role === 'assistant'" class="message-actions">
        <button aria-label="Copy response" @click="copy"><Check v-if="copied" :size="15" /><Clipboard v-else :size="15" /></button><button aria-label="Good response"><ThumbsUp :size="15" /></button><button aria-label="Bad response"><ThumbsDown :size="15" /></button><button aria-label="Regenerate response"><RotateCcw :size="15" /></button>
      </div>
    </div>
  </article>
</template>
