<script setup lang="ts">
import { ref } from 'vue'
import { AlertCircle, Check, Clipboard, RotateCcw, ThumbsDown, ThumbsUp, Sparkles, Wrench } from 'lucide-vue-next'
import type { Message } from '@/types/chat'
const props = defineProps<{ message: Message; regenerating?: boolean }>()
const emit = defineEmits<{ regenerate: [id: string]; feedback: [id: string, value: 'up' | 'down'] }>()
const copied = ref(false)
async function copy() {
  try {
    await navigator.clipboard.writeText(props.message.content)
    copied.value = true
    setTimeout(() => copied.value = false, 1300)
  } catch { copied.value = false }
}
/// Tool arguments are shown as the model sent them; a one-line form is enough
/// to see which file was touched without burying the answer.
function summarize(value: unknown) {
  if (value === undefined || value === null) return ''
  const text = typeof value === 'string' ? value : JSON.stringify(value)
  return text.length > 120 ? `${text.slice(0, 119)}\u2026` : text
}
</script>

<template>
  <article :class="['message', message.role, { 'has-error': message.error }]">
    <div v-if="message.role === 'assistant'" class="assistant-mark"><Sparkles :size="16" /></div>
    <div class="message-body">
      <p v-if="message.contextRecords" class="harness-context">Retrieved {{ message.contextRecords }} codebase record{{ message.contextRecords === 1 ? '' : 's' }} before answering.</p>
      <section v-if="message.tools?.length" class="tool-activity" aria-label="Tools the agent ran">
        <details v-for="tool in message.tools" :key="tool.id || tool.name" :class="{ failed: tool.failed }">
          <summary><Wrench :size="13" /><strong>{{ tool.name }}</strong><span>{{ summarize(tool.arguments) }}</span><em v-if="tool.failed">failed</em></summary>
          <pre>{{ tool.output ?? 'Running…' }}</pre>
        </details>
      </section>
      <div class="message-content">{{ message.content }}</div>
      <p v-if="message.incomplete" class="harness-incomplete" role="status">The agent reached its step limit, so this answer is incomplete. Ask it to continue.</p>
      <p v-if="message.error" class="message-error" role="alert"><AlertCircle :size="15" />{{ message.error }}</p>
      <div v-if="message.role === 'assistant'" class="message-actions">
        <button type="button" :aria-label="copied ? 'Response copied' : 'Copy response'" @click="copy"><Check v-if="copied" :size="15" /><Clipboard v-else :size="15" /></button>
        <button type="button" aria-label="Good response" :aria-pressed="message.feedback === 'up'" :class="{ selected: message.feedback === 'up' }" @click="emit('feedback', message.id, 'up')"><ThumbsUp :size="15" /></button>
        <button type="button" aria-label="Bad response" :aria-pressed="message.feedback === 'down'" :class="{ selected: message.feedback === 'down' }" @click="emit('feedback', message.id, 'down')"><ThumbsDown :size="15" /></button>
        <button type="button" aria-label="Regenerate response" :disabled="regenerating" @click="emit('regenerate', message.id)"><RotateCcw :size="15" /></button>
      </div>
    </div>
  </article>
</template>
