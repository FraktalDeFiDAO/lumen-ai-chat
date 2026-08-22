<script setup lang="ts">
import { nextTick, ref } from 'vue'
import { ArrowUp, Paperclip, Square, SlidersHorizontal } from 'lucide-vue-next'

const props = defineProps<{ generating?: boolean }>()
const emit = defineEmits<{ send: [value: string]; stop: [] }>()
const text = ref('')
const textarea = ref<HTMLTextAreaElement | null>(null)
function resize() { if (textarea.value) { textarea.value.style.height = 'auto'; textarea.value.style.height = `${Math.min(textarea.value.scrollHeight, 180)}px` } }
function submit() {
  if (props.generating) { emit('stop'); return }
  if (!text.value.trim()) return
  emit('send', text.value.trim()); text.value = ''; nextTick(resize)
}
function keydown(event: KeyboardEvent) { if (event.key === 'Enter' && !event.shiftKey) { event.preventDefault(); submit() } }
</script>

<template>
  <div class="composer-shell">
    <textarea ref="textarea" v-model="text" rows="1" aria-label="Message Lumen" placeholder="Message Lumen..." @input="resize" @keydown="keydown" />
    <div class="composer-actions">
      <div class="flex items-center gap-1"><button class="composer-icon" aria-label="Attach a file"><Paperclip :size="18" /></button><button class="model-button"><SlidersHorizontal :size="15" /><span>Lumen 2</span></button></div>
      <button :class="['send-button', { ready: text.trim() || generating }]" :aria-label="generating ? 'Stop generating' : 'Send message'" @click="submit"><Square v-if="generating" :size="13" fill="currentColor" /><ArrowUp v-else :size="18" /></button>
    </div>
  </div>
</template>
